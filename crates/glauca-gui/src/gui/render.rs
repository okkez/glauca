//! Top-level layout: the left/center panes and item rows, the menu bar, and the
//! `Render` impl that wires key actions and mounts the overlay/menu/dialog layers.

use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::menu::DropdownMenu;
use gpui_component::{ActiveTheme, Root, Sizable, StyledExt, h_flex, v_flex};

use glauca_core::logic::*;
use glauca_core::types::*;

use super::*;

impl GlaucaApp {
    pub(crate) fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        // Fixed header: current user's avatar + login (line 1) + display name
        // (line 2). Stays pinned while the entry list below scrolls.
        let mut avatar = Avatar::new().with_size(px(HEADER_AVATAR_PX));
        if let Some(login) = &self.current_user {
            avatar = avatar.name(login.clone());
        }
        if let Some(url) = &self.current_user_avatar_url {
            avatar = avatar.src(sized_avatar_url(url, HEADER_AVATAR_PX));
        }
        // Names both causes rather than asserting the token is missing: the lookup
        // also fails when the app starts before the network is up, and the engine
        // may still be retrying. Matches the Tauri header's wording.
        let login_line = self
            .current_user
            .clone()
            .unwrap_or_else(|| "login unknown".to_string());
        let header = h_flex()
            .w_full()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(avatar)
            .child(
                v_flex()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_color(cx.theme().sidebar_foreground)
                            .child(SharedString::from(login_line)),
                    )
                    .when_some(self.current_user_name.clone(), |e, name| {
                        e.child(
                            div()
                                .truncate()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(name)),
                        )
                    }),
            );

        // Scrollable entry list (root queries + filter streams).
        let mut col = v_flex()
            .id("left-pane")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.left_scroll);

        for (i, entry) in self.entries.iter().enumerate() {
            let selected = i == self.entry_cursor;
            let is_stream = entry.is_filter_stream();
            let is_query = matches!(entry, LeftPaneEntry::Query(_));
            let label = match entry {
                LeftPaneEntry::Query(q) => q.label.clone(),
                LeftPaneEntry::FilterStream(fs) => fs.name.clone(),
            };
            let unread = self
                .unread_counts
                .get(&entry.unread_key())
                .copied()
                .unwrap_or(0);

            let row = h_flex()
                .id(("entry", i))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .when(is_stream, |e| e.pl(px(28.)))
                .when(selected, |e| e.bg(cx.theme().list_active))
                .when(!selected, |e| e.hover(|e| e.bg(cx.theme().list_hover)))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(cx.theme().sidebar_foreground)
                        .child(SharedString::from(label)),
                )
                .when(unread > 0, |e| {
                    e.child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().accent_foreground)
                            .bg(cx.theme().accent)
                            .px_1p5()
                            .rounded_full()
                            .child(SharedString::from(unread.to_string())),
                    )
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.focus = Focus::QueryList;
                    this.select_index(i);
                    cx.notify();
                }))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                        this.focus = Focus::QueryList;
                        this.entry_cursor = i;
                        this.open_menu(
                            ev.position,
                            MenuKind::Entry { index: i, is_query },
                            window,
                            cx,
                        );
                    }),
                );

            col = col.child(row);
        }

        // Empty area below the entries: right-click → New query. Kept as its own
        // flex_1 element so a right-click on a row hits the row handler, not this.
        // It also pushes the status footer below to the bottom of the pane.
        col = col.child(
            div()
                .id("left-empty")
                .flex_1()
                .min_h(px(24.))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, ev: &MouseDownEvent, window, cx| {
                        this.open_menu(ev.position, MenuKind::NewQueryOnly, window, cx);
                    }),
                ),
        );

        // Status footer: sync state and the latest status message (the user
        // identity now lives in the header). Only shown when there is something
        // to report.
        let mut sync_bits = Vec::new();
        if self.syncing {
            sync_bits.push("syncing…".to_string());
        }
        if self.bg_sync_pending > 0 {
            sync_bits.push(format!("{} bg", self.bg_sync_pending));
        }
        // Why an `@me` filter is showing an empty list. Worth a footer of its own
        // when nothing else is happening — the empty list explains nothing by itself.
        let me_unexpanded = self.me_unexpanded();
        let has_footer = !sync_bits.is_empty() || self.status.is_some() || me_unexpanded;
        let footer = has_footer.then(|| {
            let mut footer = v_flex()
                .w_full()
                .flex_shrink_0()
                .px_3()
                .py_2()
                .gap_0p5()
                .border_t_1()
                .border_color(cx.theme().border)
                .text_xs()
                .text_color(cx.theme().muted_foreground);
            if !sync_bits.is_empty() {
                footer = footer.child(SharedString::from(sync_bits.join("  ")));
            }
            if me_unexpanded {
                footer = footer.child(div().text_color(cx.theme().warning_foreground).child(
                    SharedString::from(glauca_core::logic::ME_UNEXPANDED_WARNING),
                ));
            }
            if let Some(s) = &self.status {
                footer = footer.child(SharedString::from(s.clone()));
            }
            footer
        });

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .child(header)
            .child(col)
            .children(footer)
    }

    /// Center pane content: the selected entry's name, the inline filter input,
    /// and the (virtualized) item list. The pane frame is added by the caller.
    pub(crate) fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        v_flex()
            .size_full()
            .child(
                // Header: name of the selected query / stream.
                div()
                    .w_full()
                    .flex_shrink_0()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .truncate()
                    .text_color(cx.theme().foreground)
                    .child(SharedString::from(
                        self.selected_entry_label().unwrap_or_default(),
                    )),
            )
            .child(
                // Inline filter input (drives `recompute_filtered`).
                div()
                    .w_full()
                    .flex_shrink_0()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(Input::new(&self.filter_input)),
            )
            // Change banner: shown when a background sync brought fresh results
            // for this query that we held back. Click to apply them.
            .when(!self.pending_changes.is_empty(), |this| {
                let view = cx.entity();
                let label = self.pending_changes.banner_label();
                // Solid attention color (amber) with its matching foreground so the
                // banner clearly stands out instead of blending into the pane.
                let bg = cx.theme().warning;
                let fg = cx.theme().warning_foreground;
                let mut hover_bg = bg;
                hover_bg.l = (hover_bg.l + 0.06).min(1.0);
                this.child(
                    div()
                        .id("pending-refresh")
                        .w_full()
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap_1()
                        .px_3()
                        .py_2()
                        .bg(bg)
                        .text_sm()
                        .font_bold()
                        .text_color(fg)
                        .cursor_pointer()
                        .hover(move |e| e.bg(hover_bg))
                        .on_click(move |_, _window, cx| {
                            view.update(cx, |this, cx| this.apply_pending(cx));
                        })
                        .child(SharedString::from(format!("↻  {label} — click to refresh"))),
                )
            })
            .child(self.render_items(cx))
    }

    pub(crate) fn render_items(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // `filtered` is precomputed (`recompute_filtered`); rows are built lazily
        // per visible range by `list`, which measures variable-height rows
        // (wrapped titles) with overdraw. `items_list`'s count is kept in sync
        // by `recompute_filtered`, so indices here always map into `filtered`.
        let count = self.filtered.len();

        let container = v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .bg(cx.theme().background);

        if count == 0 {
            return container.child(
                div()
                    .p_4()
                    .text_color(cx.theme().muted_foreground)
                    .child("No items"),
            );
        }

        // The `list` render closure only receives `&App`, so it reads the view
        // entity for row data and captures it for click handlers (which mutate
        // via `update` since `cx.listener` is unavailable here).
        let view = cx.entity();
        // Parse the inline filter once per render (used to highlight matching
        // title text) and share it across all visible rows — re-parsing it per
        // row is wasted work, especially while a divider drag re-measures the
        // visible rows every frame.
        let fq = std::rc::Rc::new(FilterQuery::parse(&expand_me(
            self.current_user.as_deref(),
            &self.filter,
        )));
        container.child(
            list(self.items_list.clone(), move |ix, _window, cx| {
                view.read(cx).render_item_row(ix, &fq, &view, cx)
            })
            .flex_1(),
        )
    }

    /// Build one center-list row: optional `NEW` badge, a wrapping (multi-line)
    /// title, and a single-line meta line. Height is intentionally unconstrained
    /// so `list` grows the row to fit a wrapped title.
    pub(crate) fn render_item_row(
        &self,
        ix: usize,
        fq: &FilterQuery,
        view: &Entity<Self>,
        cx: &App,
    ) -> AnyElement {
        let Some(item) = self.filtered.get(ix).and_then(|&i| self.items.get(i)) else {
            return div().into_any_element();
        };
        // State is shown by the status icon, and the author by the avatar row
        // below, so both are dropped from this line.
        let mut meta = format!("{}/{}#{}", item.repo_owner, item.repo_name, item.number);
        if !item.labels.is_empty() {
            meta.push_str("  ·  ");
            meta.push_str(&item.labels.join(", "));
        }

        // Participants row (above the repo/meta line): author then assignees on
        // the left, reviewers (with review-state overlays) on the right.
        let reviewers = reviewer_overlays(item);
        let assignee_extra = item.assignees.len().saturating_sub(AVATAR_LIMIT);
        let reviewer_extra = reviewers.len().saturating_sub(AVATAR_LIMIT);
        let has_participants = item.author.is_some()
            || !item.assignees.is_empty()
            || !reviewers.is_empty()
            || item.comment_count > 0;

        let is_new = item.is_new;
        let selected = ix == self.item_cursor;
        let title_el = highlight_title(&item.title, fq.highlight_ranges(&item.title), cx);

        v_flex()
            .id(ix)
            .w_full()
            .px_4()
            .py_2()
            .gap_0p5()
            .border_b_1()
            .border_color(cx.theme().border)
            .cursor_pointer()
            .when(selected, |e| e.bg(cx.theme().list_active))
            // Unread rows get a faint background tint (replaces the old NEW
            // badge); selection still takes precedence.
            .when(!selected && is_new, |e| {
                let mut tint = cx.theme().accent;
                tint.a = 0.10;
                e.bg(tint)
            })
            .when(!selected, |e| e.hover(|e| e.bg(cx.theme().list_hover)))
            .on_click({
                let view = view.clone();
                move |event: &ClickEvent, _window, cx: &mut App| {
                    // Shift+click opens the row in the browser (mouse-only
                    // equivalent of the `o` key), in addition to selecting it.
                    let shift = event.modifiers().shift;
                    view.update(cx, |this, cx| {
                        this.focus = Focus::ItemList;
                        this.item_cursor = ix;
                        this.mark_current_item_read(cx);
                        this.refetch_current_body_if_missing();
                        if shift && let Some(item) = this.selected_item() {
                            this.send(EngineCommand::OpenBrowser {
                                item: Box::new(item),
                            });
                        }
                        cx.notify();
                    });
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let view = view.clone();
                move |ev: &MouseDownEvent, window, cx: &mut App| {
                    view.update(cx, |this, cx| {
                        this.focus = Focus::ItemList;
                        this.item_cursor = ix;
                        if let Some(item) = this.selected_item() {
                            this.open_menu(ev.position, MenuKind::Item(Box::new(item)), window, cx);
                        }
                        cx.notify();
                    });
                }
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    // Top-align so the status icon sits beside the first line
                    // when the title wraps to multiple lines.
                    .items_start()
                    .child(item_state_icon(item, cx))
                    .child(title_el),
            )
            .when(has_participants, |e| {
                e.child(
                    h_flex()
                        .w_full()
                        .justify_between()
                        .items_center()
                        .gap_2()
                        // Left: author, then assignees (+N overflow).
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .flex_shrink_0()
                                .when_some(item.author.as_ref(), |e, a| e.child(user_avatar(a)))
                                // Arrow reads "author → assignee(s)" when both sides exist.
                                .when(item.author.is_some() && !item.assignees.is_empty(), |e| {
                                    e.child(
                                        div()
                                            .flex_shrink_0()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child("→"),
                                    )
                                })
                                .children(item.assignees.iter().take(AVATAR_LIMIT).map(user_avatar))
                                .when(assignee_extra > 0, |e| {
                                    e.child(avatar_overflow(assignee_extra, cx))
                                }),
                        )
                        // Right: reviewers with review-state overlay (+N overflow),
                        // then the comment count (octicon + number) when nonzero.
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .flex_shrink_0()
                                .children(
                                    reviewers
                                        .iter()
                                        .take(AVATAR_LIMIT)
                                        .map(|(u, s)| reviewer_avatar(u, *s, cx)),
                                )
                                .when(reviewer_extra > 0, |e| {
                                    e.child(avatar_overflow(reviewer_extra, cx))
                                })
                                .when(item.comment_count > 0, |e| {
                                    e.child(
                                        h_flex()
                                            .gap_0p5()
                                            .items_center()
                                            .flex_shrink_0()
                                            .child(
                                                svg()
                                                    .path("octicons/comment.svg")
                                                    .size_3()
                                                    .flex_shrink_0()
                                                    .text_color(cx.theme().muted_foreground),
                                            )
                                            .child(
                                                div()
                                                    .text_xs()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(SharedString::from(
                                                        item.comment_count.to_string(),
                                                    )),
                                            ),
                                    )
                                }),
                        ),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_1()
                    .items_center()
                    // Private repos get a lock glyph ahead of the "owner/name" text.
                    .when(item.repo_private, |e| {
                        e.child(
                            svg()
                                .path("octicons/lock.svg")
                                .size_3()
                                .flex_shrink_0()
                                .text_color(cx.theme().muted_foreground),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(meta)),
                    )
                    // Relative update time, right-aligned at the row's end.
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(glauca_core::time::format_relative_time(
                                &item.updated_at,
                            ))),
                    ),
            )
            .into_any_element()
    }

    /// Top dropdown menu bar. Deliberately minimal — item/entry actions stay on the
    /// keyboard and right-click menus, not here. Only app-level commands: a Glauca
    /// (app) menu and a Help menu.
    pub(crate) fn render_menu_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let app = cx.entity();

        // Glauca (app) menu: re-sync the selected query, then quit.
        let glauca_app = app.clone();
        let glauca_menu = Button::new("menu-glauca")
            .small()
            .ghost()
            .label("Glauca")
            .dropdown_menu(move |menu, _w, _cx| {
                let mut menu = app_menu_item(menu, &glauca_app, "Sync now", |this, _w, cx| {
                    this.select_current_entry(true);
                    cx.notify();
                });
                menu = app_menu_item(menu, &glauca_app, "Full resync", |this, _w, cx| {
                    this.full_resync_current();
                    cx.notify();
                });
                menu = menu.separator();
                app_menu_item(menu, &glauca_app, "Quit", |this, w, cx| {
                    this.on_quit(&Quit, w, cx)
                })
            });

        // View menu: theme selection (System / Light / Dark). The active choice
        // is marked with a leading check; the rest are blank-padded to align.
        let view_app = app.clone();
        let current_theme = self.settings.theme;
        let notifications_enabled = self.settings.notifications_enabled;
        let theme_label = move |pref: ThemePreference, text: &str| {
            let mark = if pref == current_theme { "✓ " } else { "   " };
            format!("{mark}{text}")
        };
        let view_menu = Button::new("menu-view")
            .small()
            .ghost()
            .label("View")
            .dropdown_menu(move |menu, _w, _cx| {
                let menu = [
                    (ThemePreference::System, "Theme: System"),
                    (ThemePreference::Light, "Theme: Light"),
                    (ThemePreference::Dark, "Theme: Dark"),
                ]
                .into_iter()
                .fold(menu, |menu, (pref, text)| {
                    app_menu_item(
                        menu,
                        &view_app,
                        theme_label(pref, text),
                        move |this, w, cx| this.set_theme(pref, w, cx),
                    )
                });
                let menu = menu.separator();
                let notif_mark = if notifications_enabled { "✓ " } else { "   " };
                app_menu_item(
                    menu,
                    &view_app,
                    format!("{notif_mark}Desktop notifications"),
                    |this, _w, cx| this.toggle_notifications(cx),
                )
            });

        // Help menu: About (version) and a keyboard-shortcuts reference.
        let help_app = app.clone();
        let help_menu = Button::new("menu-help")
            .small()
            .ghost()
            .label("Help")
            .dropdown_menu(move |menu, _w, _cx| {
                let mut menu = app_menu_item(menu, &help_app, "About", |this, w, cx| {
                    this.open_about_dialog(w, cx)
                });
                menu = app_menu_item(menu, &help_app, "Keyboard shortcuts", |this, w, cx| {
                    this.open_shortcuts_dialog(w, cx)
                });
                menu
            });

        h_flex()
            .w_full()
            .flex_shrink_0()
            .px_2()
            .py_1()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(glauca_menu)
            .child(view_menu)
            .child(help_menu)
    }
}

impl GlaucaApp {
    /// Perf diagnostics (RUST_LOG=glauca_gui=debug): log the gap between
    /// element tree rebuilds. During a burst of repaints (key held down) a
    /// growing gap means layout/paint can't keep up with input. The timestamp
    /// is updated unconditionally — every frame, even with debug logging off —
    /// so the first gap after enabling logging is still correct.
    fn log_frame_gap(&mut self) {
        let now = std::time::Instant::now();
        let Some(prev) = self.last_render_at.replace(now) else {
            return;
        };
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let gap_ms = now.duration_since(prev).as_millis() as u64;
        // Gaps over a second are idle time, not slowness — skip them to keep
        // the log readable.
        if gap_ms <= 1000 {
            tracing::debug!(gap_ms, "frame");
        }
    }
}

impl Render for GlaucaApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.log_frame_gap();
        v_flex()
            .id("glauca-root")
            .key_context(GLAUCA_CONTEXT)
            .track_focus(&self.focus_handle)
            // Track the pointer (no repaint) so the Enter action menu can anchor
            // near the cursor.
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, _cx| {
                this.last_pointer = ev.position;
            }))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_focus_left))
            .on_action(cx.listener(Self::on_focus_right))
            .on_action(cx.listener(Self::on_activate))
            .on_action(cx.listener(Self::on_focus_filter))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_delete_entry))
            .on_action(cx.listener(Self::on_reorder_down))
            .on_action(cx.listener(Self::on_reorder_up))
            .on_action(cx.listener(Self::on_new_query))
            .on_action(cx.listener(Self::on_new_filter_stream))
            .on_action(cx.listener(Self::on_edit_entry))
            .on_action(cx.listener(Self::on_open_in_browser))
            .on_action(cx.listener(Self::on_copy_url))
            .on_action(cx.listener(Self::on_run_custom_action))
            .on_action(cx.listener(Self::on_refresh))
            .on_action(cx.listener(Self::on_open_comments))
            .on_action(cx.listener(Self::on_comments_scroll_down))
            .on_action(cx.listener(Self::on_comments_scroll_up))
            .on_action(cx.listener(Self::on_comments_top))
            .on_action(cx.listener(Self::on_comments_bottom))
            .on_action(cx.listener(Self::on_comments_toggle_sort))
            .on_action(cx.listener(Self::on_comments_toggle_hidden))
            .on_action(cx.listener(Self::on_comments_close))
            .on_action(cx.listener(Self::on_set_theme_system))
            .on_action(cx.listener(Self::on_set_theme_light))
            .on_action(cx.listener(Self::on_set_theme_dark))
            .on_action(cx.listener(Self::on_toggle_notifications))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_menu_bar(cx))
            .child(
                // Drag-resizable 3-pane row. The group container is `size_full`,
                // so it's wrapped in a `flex_1`/`min_h_0` div to take the height
                // left under the menu bar. The left/right panes carry explicit
                // (persisted) widths and MUST be `.flex_none()` — otherwise their
                // internal `flex_grow: 1` makes them absorb space during a drag,
                // so resizing the center/right divider would visibly stretch the
                // *left* pane instead. Only the center stays flexible (it soaks
                // up whatever the sized panels don't take). Every drag persists
                // all sizes via `on_resize`.
                div().w_full().flex_1().min_h_0().child(
                    h_resizable("panes")
                        .with_state(&self.pane_state)
                        .on_resize({
                            // Mirror the drag into the in-memory settings (the
                            // single source of truth) and let the debounced task
                            // flush once the drag pauses — no disk I/O per event.
                            let this = cx.entity().downgrade();
                            move |state, _window, cx| {
                                let sizes: Vec<f32> = state
                                    .read(cx)
                                    .sizes()
                                    .iter()
                                    .map(|p| f32::from(*p))
                                    .collect();
                                let _ = this.update(cx, |app, cx| {
                                    app.settings.pane_sizes = sizes;
                                    app.schedule_settings_save(cx);
                                });
                            }
                        })
                        .child(
                            resizable_panel()
                                .size(px(self
                                    .settings
                                    .pane_sizes
                                    .first()
                                    .copied()
                                    .unwrap_or(280.)))
                                .size_range(px(250.)..px(560.))
                                .flex_none()
                                .child(pane_frame(
                                    self.focus == Focus::QueryList,
                                    self.render_left(cx),
                                    cx,
                                )),
                        )
                        .child(
                            resizable_panel().size_range(px(250.)..px(1000.)).child(
                                pane_frame(
                                    self.focus == Focus::ItemList,
                                    self.render_center(cx),
                                    cx,
                                )
                                // The center pane is the flexible one; allow it to
                                // shrink below its content width as panels resize.
                                .min_w_0(),
                            ),
                        )
                        .child(
                            resizable_panel()
                                .size(px(self.settings.pane_sizes.get(2).copied().unwrap_or(440.)))
                                .size_range(px(300.)..px(2400.))
                                .flex_none()
                                .child(pane_frame(
                                    self.focus == Focus::ItemDetail,
                                    self.render_detail(cx),
                                    cx,
                                )),
                        ),
                ),
            )
            // Comments overlay draws over the 3-pane row (absolute, full-size).
            .when(self.comments_open, |this| {
                this.child(self.render_comments_overlay(cx))
            })
            // Right-click / Enter action menu, anchored at the click/pointer point.
            // A full-window backdrop swallows clicks and dismisses on outside click.
            .when_some(self.menu.clone(), |this, menu| {
                this.child(
                    deferred(
                        anchored().child(
                            div()
                                .w(window.bounds().size.width)
                                .h(window.bounds().size.height)
                                .occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _, _, cx| {
                                        this.menu = None;
                                        cx.notify();
                                    }),
                                )
                                .child(
                                    anchored()
                                        .position(self.menu_pos)
                                        .snap_to_window_with_margin(px(8.))
                                        .child(menu),
                                ),
                        ),
                    )
                    .with_priority(1),
                )
            })
            // gpui-component stores open dialogs/sheets/notifications in `Root`, but
            // `Root`'s own render does NOT paint them — the inner view must mount the
            // overlay layers (see examples/dialog_overlay). Without these, every
            // `open_dialog` (entry add/edit forms, action menu, comment/approve/merge)
            // is invisible, which is why the editing keys appeared to do nothing.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_sheet_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}
