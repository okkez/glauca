//! Construction and theming: the `GlaucaApp::new` bootstrap (engine wiring, the
//! push-based message delivery loop, filter-input subscription, settings
//! restore) and the theme /
//! settings-save / notification-toggle handlers.

use gpui::*;
use gpui_component::input::{InputEvent, InputState};
use gpui_component::resizable::ResizableState;
use gpui_component::text::TextViewState;
use gpui_component::{ActiveTheme, Theme, ThemeMode};
use smol::Timer;

use glauca_core::actions::CustomActions;
use glauca_core::engine::{Engine, EngineInit};
use glauca_core::notify::ItemTracker;

use super::settings::{GuiSettings, ThemePreference};
use super::*;

impl GlaucaApp {
    pub(crate) fn new(
        engine: Engine,
        init: EngineInit,
        settings: GuiSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Deliver engine messages push-based: await the channel and repaint as
        // soon as a message lands, like the TUI/Tauri front-ends. The old loop
        // polled `try_recv` on a 50ms timer, which added up to 50ms of latency
        // to every engine round trip (left-pane navigation reloads items from
        // SQLite on each move) and woke the event loop 20×/s while idle.
        // Awaiting a tokio mpsc receiver needs no tokio runtime context — the
        // sender wakes the task directly, and gpui reschedules it on the main
        // thread. After the first message, drain whatever else is queued so a
        // background-sync burst is applied in one frame with a single repaint.
        // `spawn_in` (not `spawn`) so `apply` gets a `&mut Window`: error
        // messages surface as `push_notification` toasts, which need the window.
        let cmd_tx = engine.sender();
        cx.spawn_in(window, async move |this, cx| {
            let mut engine = engine;
            while let Some(first) = engine.recv().await {
                let result = this.update_in(cx, |this, window, cx| {
                    let t = std::time::Instant::now();
                    let mut n = 1;
                    this.apply(first, window, cx);
                    while let Some(msg) = engine.try_recv() {
                        this.apply(msg, window, cx);
                        n += 1;
                    }
                    cx.notify();
                    tracing::debug!(
                        batch = n,
                        apply_us = t.elapsed().as_micros() as u64,
                        "engine batch"
                    );
                });
                if result.is_err() {
                    // Entity gone (window closed) — stop the loop.
                    break;
                }
            }
        })
        .detach();
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter…"));
        // Mirror the input value into `filter` (and reset the item cursor) on every
        // change so `recompute_filtered` re-runs and the detail pane stays in range.
        let subscription = cx.subscribe_in(
            &filter_input,
            window,
            |this, input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.filter = input.read(cx).value().to_string();
                    // Debounce: re-filter only after typing pauses. Replacing the
                    // task drops (cancels) any still-pending one.
                    this.filter_task = Some(cx.spawn(async move |this, cx| {
                        Timer::after(FILTER_DEBOUNCE).await;
                        let _ = this.update(cx, |this, cx| {
                            this.item_cursor = 0;
                            this.reset_detail_scroll();
                            this.recompute_filtered();
                            cx.notify();
                        });
                    }));
                }
            },
        );
        let EngineInit {
            entries,
            current_user,
            current_user_name,
            current_user_avatar_url,
        } = init;
        let pane_state = cx.new(|_| ResizableState::default());
        let detail_text = cx.new(|cx| TextViewState::markdown("", cx));
        let mut app = Self {
            cmd_tx,
            entries,
            entry_cursor: 0,
            current_user,
            current_user_name,
            current_user_avatar_url,
            items: Vec::new(),
            filtered: Vec::new(),
            item_cursor: 0,
            filter: String::new(),
            unread_counts: HashMap::new(),
            stream_filter: None,
            body_refresh_requested: HashSet::new(),
            pending_items: None,
            pending_changes: ChangeCounts::default(),
            syncing: false,
            bg_sync_pending: 0,
            status: None,
            left_scroll: ScrollHandle::new(),
            items_list: ListState::new(0, ListAlignment::Top, px(120.)),
            pane_state,
            settings,
            settings_save_task: None,
            detail_text,
            detail_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            focus: Focus::QueryList,
            comments_open: false,
            comments: Vec::new(),
            comments_loading: false,
            comments_scroll: ScrollHandle::new(),
            comments_sort_desc: false,
            comments_show_hidden: false,
            comments_title: SharedString::default(),
            comments_focus_handle: cx.focus_handle(),
            menu: None,
            menu_pos: point(px(0.), px(0.)),
            last_pointer: point(px(0.), px(0.)),
            review_action: ReviewEvent::Approve,
            filter_stream_form: None,
            filter_input,
            filter_task: None,
            notif_tracker: ItemTracker::new(),
            custom_actions: CustomActions::load(),
            _subscriptions: vec![subscription],
            last_render_at: None,
        };
        // Apply the saved theme up front (System follows the OS appearance).
        app.apply_theme(Some(window), cx);
        // While following the OS, re-sync whenever its appearance flips. The
        // closure re-reads the theme setting so pinning Light/Dark stops the follow.
        let this = cx.entity();
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            this.update(cx, |app, cx| {
                if app.settings.theme == ThemePreference::System {
                    // Re-apply via `apply_theme` so the GitHub dark overlay is
                    // re-applied when the OS flips to dark.
                    app.apply_theme(Some(window), cx);
                }
            });
        });
        app._subscriptions.push(appearance_sub);
        // Flush pending settings whenever the app quits. Every quit trigger funnels
        // through `cx.quit()` → `shutdown()`, which runs quit observers synchronously
        // before dropping the entity, so the write always completes. On non-macOS,
        // closing the last window quits the app, so an OS-initiated close (title-bar
        // ×, Alt-F4) reaches this hook too; the `q`/menu Quit action reaches it via
        // `cx.quit()`. (On macOS the sole window closing leaves the app running with
        // settings still in memory — the eventual Cmd-Q flushes them.) Without this
        // hook, a change made inside the debounce window right before an OS-initiated
        // quit would be lost — a regression from the old eager per-event save.
        let quit_sub = cx.on_app_quit(|app, _cx| {
            // Cancel the still-pending debounce first so it can't race this write,
            // then flush once synchronously.
            app.settings_save_task = None;
            app.settings.save();
            async {}
        });
        app._subscriptions.push(quit_sub);
        app.prime();
        // Restore saved column widths into the authoritative ResizableState after
        // the first frame is drawn (panels are synced and the container has a real
        // size by then). The `.size()` initial_size hints on the panels lose to
        // `adjust_to_container_size`, which overwrites `panel.size` on that first
        // prepaint — so seed the widths explicitly here. `on_next_frame` is a
        // one-shot, so no guard flag is needed.
        let pane_state = app.pane_state.clone();
        let left = app.settings.pane_sizes.first().copied();
        let right = app.settings.pane_sizes.get(2).copied();
        if left.is_some() || right.is_some() {
            window.on_next_frame(move |window, cx| {
                pane_state.update(cx, |state, cx| {
                    // panels.len() == 3 and the container size is settled here;
                    // out-of-range indices are a no-op. Apply left (ix 0) before
                    // right (ix 2); both take from the flexible center pane.
                    if let Some(w) = left {
                        state.resize_panel(0, px(w), window, cx);
                    }
                    if let Some(w) = right {
                        state.resize_panel(2, px(w), window, cx);
                    }
                });
            });
        }
        // Grab keyboard focus so single-letter navigation works without a click.
        app.focus_handle.focus(window, cx);
        app
    }

    /// Apply `self.settings.theme` to the global gpui-component theme. `System`
    /// follows the OS appearance; `Light`/`Dark` pin an explicit mode. When the
    /// resolved mode is dark, overlay the GitHub-flavored palette (the stock
    /// dark theme is near-black) — see `apply_github_dark_overlay`.
    pub(crate) fn apply_theme(&self, window: Option<&mut Window>, cx: &mut App) {
        match self.settings.theme {
            ThemePreference::System => Theme::sync_system_appearance(window, cx),
            ThemePreference::Light => Theme::change(ThemeMode::Light, window, cx),
            ThemePreference::Dark => Theme::change(ThemeMode::Dark, window, cx),
        }
        if cx.theme().mode.is_dark() {
            apply_github_dark_overlay(cx);
        }
    }

    /// Flush the in-memory settings to disk after a short idle delay, off the UI
    /// thread. Replacing the task cancels a still-pending flush (same pattern as
    /// `filter_task`), so a burst of changes — a pane drag most of all — writes
    /// once. The `on_app_quit` hook flushes synchronously so a change made inside
    /// the debounce window right before quitting isn't lost.
    pub(crate) fn schedule_settings_save(&mut self, cx: &mut Context<Self>) {
        self.settings_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTINGS_SAVE_DEBOUNCE).await;
            let Ok(settings) = this.update(cx, |this, _| this.settings.clone()) else {
                return; // entity gone; the on_app_quit hook already flushed on quit
            };
            cx.background_executor()
                .spawn(async move { settings.save() })
                .await;
        }));
    }

    /// Switch the theme from the View menu: apply it, schedule a save, repaint.
    pub(crate) fn set_theme(
        &mut self,
        pref: ThemePreference,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.theme = pref;
        self.apply_theme(Some(window), cx);
        self.schedule_settings_save(cx);
        cx.notify();
    }

    pub(crate) fn on_set_theme_system(
        &mut self,
        _: &SetThemeSystem,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_theme(ThemePreference::System, window, cx);
    }

    pub(crate) fn on_set_theme_light(
        &mut self,
        _: &SetThemeLight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_theme(ThemePreference::Light, window, cx);
    }

    pub(crate) fn on_set_theme_dark(
        &mut self,
        _: &SetThemeDark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_theme(ThemePreference::Dark, window, cx);
    }

    /// Toggle desktop notifications from the View menu: flip the flag, schedule
    /// a save, and repaint the menu marker.
    pub(crate) fn toggle_notifications(&mut self, cx: &mut Context<Self>) {
        self.settings.notifications_enabled = !self.settings.notifications_enabled;
        self.schedule_settings_save(cx);
        cx.notify();
    }

    pub(crate) fn on_toggle_notifications(
        &mut self,
        _: &ToggleNotifications,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_notifications(cx);
    }
}
