//! Modal dialogs: GitHub action prompts (comment / review / merge) plus the
//! About and Shortcuts info dialogs.

use gpui::*;
use gpui_component::input::{Textarea, TextareaState};
use gpui_component::radio::RadioGroup;

use glauca_core::engine::{EngineCommand, ReviewEvent};
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    pub(crate) fn open_comment_dialog(
        &mut self,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .placeholder("Comment body")
        });
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let body_c = body.clone();
            let body_ok = body.clone();
            let this = this.clone();
            let item = item.clone();
            dlg.title("Comment")
                .w(px(560.))
                .content(move |content, _w, _cx| content.child(Textarea::new(&body_c).h(px(220.))))
                .on_ok(move |_, _w, cx| {
                    let b = body_ok.read(cx).value().to_string();
                    let b = b.trim().to_string();
                    if !b.is_empty()
                        && let Some(app) = this.upgrade()
                    {
                        let item = item.clone();
                        app.update(cx, |app, _| {
                            app.send(EngineCommand::Comment {
                                url: item.url.clone(),
                                kind: item.kind.clone(),
                                body: b,
                            })
                        });
                    }
                    true
                })
        });
    }

    /// Submit a PR review: pick Comment / Approve / Request changes, type an optional body,
    /// and Cancel / Submit. Radio order matches `review_action`'s index mapping below.
    /// Explicit buttons rather than `on_ok` so the actions are visible.
    pub(crate) fn open_review_dialog(
        &mut self,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.review_action = ReviewEvent::Approve;
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 12)
                .placeholder("Review comment (required for Comment / Request changes)")
        });
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let body_render = body.clone();
            let body_submit = body.clone();
            let this = this.clone();
            let item = item.clone();
            dlg.title("Submit review")
                .w(px(560.))
                .content(move |content, _w, cx| {
                    // Radio order ⇄ ReviewEvent: 0 Comment, 1 Approve, 2 Request changes.
                    let selected = this.upgrade().map(|app| match app.read(cx).review_action {
                        ReviewEvent::Comment => 0,
                        ReviewEvent::Approve => 1,
                        ReviewEvent::RequestChanges => 2,
                    });
                    let radios = RadioGroup::horizontal("review-action")
                        .children(["Comment", "Approve", "Request changes"])
                        .selected_index(selected)
                        .on_click({
                            let this = this.clone();
                            move |ix, _w, cx| {
                                let event = match *ix {
                                    0 => ReviewEvent::Comment,
                                    2 => ReviewEvent::RequestChanges,
                                    _ => ReviewEvent::Approve,
                                };
                                if let Some(app) = this.upgrade() {
                                    app.update(cx, |app, cx| {
                                        app.review_action = event;
                                        cx.notify();
                                    });
                                }
                            }
                        });
                    let buttons = h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("review-cancel")
                                .ghost()
                                .label("Cancel")
                                .on_click(move |_, window, cx| {
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("review-submit")
                                .primary()
                                .label("Submit review")
                                .on_click({
                                    let this = this.clone();
                                    let item = item.clone();
                                    let body_submit = body_submit.clone();
                                    move |_, window, cx| {
                                        let Some(app) = this.upgrade() else { return };
                                        let event = app.read(cx).review_action;
                                        let b = body_submit.read(cx).value().trim().to_string();
                                        // gh requires a body for comment / request-changes reviews.
                                        if event.requires_body() && b.is_empty() {
                                            app.update(cx, |app, cx| {
                                                app.status = Some(
                                        "Review comment required for Comment / Request changes"
                                            .to_string(),
                                    );
                                                cx.notify();
                                            });
                                            return;
                                        }
                                        let body = if b.is_empty() { None } else { Some(b) };
                                        window.close_dialog(cx);
                                        app.update(cx, |app, _| {
                                            app.send(EngineCommand::SubmitReview {
                                                url: item.url.clone(),
                                                event,
                                                body,
                                            })
                                        });
                                    }
                                }),
                        );
                    content
                        .gap_3()
                        .child(radios)
                        .child(Textarea::new(&body_render).h(px(180.)))
                        .child(buttons)
                })
        });
    }

    pub(crate) fn open_merge_dialog(
        &mut self,
        item: ItemEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let this = cx.weak_entity();
        window.open_dialog(cx, move |dlg, _w, _cx| {
            let this = this.clone();
            let item = item.clone();
            dlg.title("Merge strategy")
                .w(px(320.))
                .content(move |content, _w, _cx| {
                    let mut col = content.gap_2();
                    for (ix, strat) in MergeStrategy::all().into_iter().enumerate() {
                        let label = strat.label().to_string();
                        let item = item.clone();
                        let this = this.clone();
                        col = col.child(Button::new(("merge", ix)).label(label).on_click(
                            move |_, window, cx| {
                                let item = item.clone();
                                let strat = strat.clone();
                                let this = this.clone();
                                window.close_dialog(cx);
                                if let Some(app) = this.upgrade() {
                                    app.update(cx, |app, _| {
                                        app.send(EngineCommand::Merge {
                                            url: item.url.clone(),
                                            strategy: strat,
                                        })
                                    });
                                }
                            },
                        ));
                    }
                    col
                })
        });
    }

    /// Help → About: a small informational dialog with the app version, dismissed by the
    /// default OK button.
    pub(crate) fn open_about_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, move |dlg, _w, _cx| {
            dlg.title("About Glauca")
                .w(px(360.))
                .content(move |content, _w, _cx| {
                    content
                        .gap_1()
                        .text_sm()
                        .child(SharedString::from(format!(
                            "glauca-gui {}",
                            env!("CARGO_PKG_VERSION")
                        )))
                        .child(SharedString::from(
                            "GitHub PR/issue triage for the terminal.",
                        ))
                })
        });
    }

    /// Help → Keyboard shortcuts: a read-only two-column list of the key bindings.
    pub(crate) fn open_shortcuts_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.open_dialog(cx, move |dlg, _w, _cx| {
            dlg.title("Keyboard shortcuts")
                .w(px(480.))
                .content(move |content, _w, _cx| {
                    let mut list = content.gap_1().text_sm();
                    for (key, desc) in SHORTCUTS {
                        if desc.is_empty() {
                            list = list
                                .child(div().pt_2().font_bold().child(SharedString::from(*key)));
                        } else {
                            list = list.child(
                                h_flex()
                                    .w_full()
                                    .gap_3()
                                    .child(
                                        div()
                                            .flex_shrink_0()
                                            .w(px(160.))
                                            .child(SharedString::from(*key)),
                                    )
                                    .child(
                                        div().flex_1().min_w_0().child(SharedString::from(*desc)),
                                    ),
                            );
                        }
                    }
                    list
                })
        });
    }
}

/// Rows shown in the Help → Keyboard shortcuts dialog (`key`, `description`). An
/// empty description marks a section-header row. Kept in sync by hand with the
/// `KeyBinding::new(...)` table registered in `main()`.
pub(crate) const SHORTCUTS: &[(&str, &str)] = &[
    (
        "j / k  ·  ↓ / ↑",
        "Move cursor down / up (detail pane: scroll the body)",
    ),
    ("h / l  ·  ← / →", "Focus previous / next pane"),
    ("Enter", "Activate (commit selection / item action menu)"),
    ("/", "Focus the filter input"),
    ("Esc", "Cancel / close overlay / leave filter"),
    ("n", "New query"),
    ("f", "New filter stream"),
    ("e", "Edit selected entry"),
    ("d", "Delete selected entry"),
    ("Shift+J / Shift+K", "Reorder selected entry down / up"),
    ("o", "Open selected item in browser"),
    ("c", "View comments for selected item"),
    ("y", "Copy selected item URL to clipboard"),
    ("x", "Run a custom action on selected item"),
    ("r", "Refresh selected list (left pane) or item"),
    ("q", "Quit"),
    ("Comments overlay", ""),
    ("j / k  ·  ↓ / ↑", "Scroll comments"),
    ("g / Shift+G", "Jump to top / bottom"),
    ("s", "Toggle sort order"),
    ("h", "Show / hide minimized comments"),
    ("q / Esc", "Close comments"),
];
