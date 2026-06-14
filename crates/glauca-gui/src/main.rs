//! glauca-gui — gpui front-end for glauca (phase B, MVP 閲覧先行).
//!
//! gpui owns the main-thread event loop and is not tokio-aware, so the async
//! engine runs on a separate multi-thread tokio runtime. The view periodically
//! drains `engine.try_recv()` and repaints; commands are sent from non-async
//! click handlers via a cloned `EngineCommand` sender (`engine.sender()`).
//!
//! B1: two panes. Left = the left-pane entries (root queries + indented filter
//! streams) as a clickable list with selection highlight and unread badges.
//! Center = the cached item list for the selected entry, with `NEW` badges and
//! scrolling. Selecting an entry mirrors the TUI `select_current_entry` flow:
//! load cached items, mark the entry viewed, and (for root queries) sync.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use glauca_core::engine::{AppMessage, Engine, EngineCommand, EngineInit};
use glauca_core::logic::{compute_unread_counts, filter_items, is_item_new_since};
use glauca_core::types::{ItemEntry, LeftPaneEntry};
use glauca_core::{db, github};
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{h_flex, v_flex, ActiveTheme, Root, StyledExt};
use smol::Timer;
use tokio::sync::mpsc::Sender;

/// How often the GUI drains engine messages and repaints.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

struct GlaucaApp {
    engine: Engine,
    /// Cloneable command sender, used from non-async click handlers.
    cmd_tx: Sender<EngineCommand>,

    entries: Vec<LeftPaneEntry>,
    entry_cursor: usize,
    current_user: Option<String>,

    items: Vec<ItemEntry>,
    unread_counts: HashMap<i64, usize>,
    /// Filter stream filter applied to the item list (None for root queries).
    stream_filter: Option<String>,
    /// `last_viewed_at` of the selected entry at selection time; drives `is_new`.
    active_entry_last_viewed_at: Option<String>,

    /// Whether a manual GitHub sync is in progress for the selected query.
    syncing: bool,
    /// Number of pending background auto-refresh jobs (queued + in-progress).
    bg_sync_pending: usize,
    status: Option<String>,

    left_scroll: ScrollHandle,
}

impl GlaucaApp {
    fn new(engine: Engine, init: EngineInit, _window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Periodically drain engine messages and repaint while the window lives.
        cx.spawn(async move |this, cx| {
            loop {
                Timer::after(DRAIN_INTERVAL).await;
                let result = this.update(cx, |this, cx| {
                    let mut changed = false;
                    while let Some(msg) = this.engine.try_recv() {
                        this.apply(msg);
                        changed = true;
                    }
                    if changed {
                        cx.notify();
                    }
                });
                if result.is_err() {
                    // Entity gone (window closed) — stop the loop.
                    break;
                }
            }
        })
        .detach();

        let cmd_tx = engine.sender();
        let EngineInit {
            entries,
            current_user,
        } = init;
        let mut app = Self {
            engine,
            cmd_tx,
            entries,
            entry_cursor: 0,
            current_user,
            items: Vec::new(),
            unread_counts: HashMap::new(),
            stream_filter: None,
            active_entry_last_viewed_at: None,
            syncing: false,
            bg_sync_pending: 0,
            status: None,
            left_scroll: ScrollHandle::new(),
        };
        app.prime();
        app
    }

    /// Mirror of the TUI run_app startup: prime unread counts for every root
    /// query, load the initially selected entry, and enqueue the rest for
    /// background refresh.
    fn prime(&mut self) {
        let root_ids: Vec<i64> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                LeftPaneEntry::Query(q) => Some(q.id),
                LeftPaneEntry::FilterStream(_) => None,
            })
            .collect();
        for id in &root_ids {
            self.send(EngineCommand::LoadCached {
                query_id: *id,
                highlight_since: None,
            });
        }

        let initially_synced_id = if self.entries.is_empty() {
            None
        } else {
            self.select_current_entry(false)
        };

        self.send(EngineCommand::EnqueueStale {
            skip_query_id: initially_synced_id,
        });
    }

    /// Send a command to the engine. Errors (channel closed/full) are ignored,
    /// matching the engine's own fire-and-forget semantics.
    fn send(&self, cmd: EngineCommand) {
        let _ = self.cmd_tx.try_send(cmd);
    }

    fn selected_root_query_id(&self) -> Option<i64> {
        self.entries.get(self.entry_cursor).map(|e| e.root_query_id())
    }

    /// Select the entry at `index`, clearing the current item view first. Always
    /// syncs root queries (matches the TUI's explicit-selection behaviour).
    fn select_index(&mut self, index: usize) {
        if index >= self.entries.len() {
            return;
        }
        self.entry_cursor = index;
        self.items.clear();
        self.select_current_entry(true);
    }

    /// Issue the engine commands to (re)load the currently selected entry: load
    /// cached items, mark it viewed, and—for root queries—sync. Returns the root
    /// query id when a query (not a filter stream) was selected, so the caller
    /// can skip it from the background-refresh sweep.
    fn select_current_entry(&mut self, always_sync: bool) -> Option<i64> {
        let entry = self.entries.get(self.entry_cursor)?.clone();
        let viewed_at = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        let highlight_since = entry.last_viewed_at().map(str::to_string);

        self.stream_filter = entry.stream_filter().map(|s| s.to_string());
        self.active_entry_last_viewed_at = highlight_since.clone();
        if let Some(selected) = self.entries.get_mut(self.entry_cursor) {
            selected.set_last_viewed_at(Some(viewed_at.clone()));
        }
        self.unread_counts.insert(entry.id(), 0);

        let root_id = entry.root_query_id();
        self.send(EngineCommand::LoadCached {
            query_id: root_id,
            highlight_since: highlight_since.clone(),
        });
        self.send(EngineCommand::MarkEntryViewed {
            entry_id: entry.id(),
            is_filter_stream: entry.is_filter_stream(),
            viewed_at,
        });
        if entry.is_filter_stream() {
            return None;
        }

        let query_str = entry.root_query_str().unwrap_or_default().to_string();
        if always_sync {
            self.send(EngineCommand::Sync {
                query_id: root_id,
                query_str,
                highlight_since,
            });
            self.syncing = true;
        } else {
            self.send(EngineCommand::SyncIfStale {
                query_id: root_id,
                query_str,
                highlight_since,
            });
        }
        Some(root_id)
    }

    fn recompute_unread(&mut self, query_id: i64, items: &[ItemEntry]) {
        for (entry_id, unread) in
            compute_unread_counts(&self.entries, query_id, items, self.current_user.as_deref())
        {
            self.unread_counts.insert(entry_id, unread);
        }
    }

    /// Apply a single engine message to GUI state.
    fn apply(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::ItemsLoaded { query_id, mut items } => {
                self.recompute_unread(query_id, &items);
                if self.selected_root_query_id() == Some(query_id) {
                    let highlight_since = self.active_entry_last_viewed_at.clone();
                    for item in &mut items {
                        item.is_new =
                            is_item_new_since(&item.cached_at, highlight_since.as_deref());
                    }
                    self.items = items;
                }
            }
            AppMessage::EntryViewed { entry_id, viewed_at } => {
                if let Some(entry) = self.entries.iter_mut().find(|e| e.id() == entry_id) {
                    entry.set_last_viewed_at(Some(viewed_at));
                }
            }
            AppMessage::SyncStarted { .. } => self.syncing = true,
            AppMessage::SyncDone { count, .. } => {
                self.syncing = false;
                self.status = Some(format!("Synced {count} items"));
            }
            AppMessage::SyncError { error, .. } => {
                self.syncing = false;
                self.status = Some(format!("Sync error: {error}"));
            }
            AppMessage::BgSyncQueued(n) => self.bg_sync_pending += n,
            AppMessage::BgSyncJobDone => {
                self.bg_sync_pending = self.bg_sync_pending.saturating_sub(1);
            }
            AppMessage::Status(s) => self.status = Some(s),
            _ => {}
        }
    }

    fn render_left(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = v_flex()
            .id("left-pane")
            .w(px(280.))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .track_scroll(&self.left_scroll)
            .border_r_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar);

        for (i, entry) in self.entries.iter().enumerate() {
            let selected = i == self.entry_cursor;
            let is_stream = entry.is_filter_stream();
            let label = match entry {
                LeftPaneEntry::Query(q) => q.label.clone(),
                LeftPaneEntry::FilterStream(fs) => fs.name.clone(),
            };
            let unread = self.unread_counts.get(&entry.id()).copied().unwrap_or(0);

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
                    this.select_index(i);
                    cx.notify();
                }));

            col = col.child(row);
        }

        col
    }

    fn render_items(&self, cx: &mut Context<Self>) -> impl IntoElement {
        // Count only — the actual row elements are built lazily per visible
        // range by `uniform_list` below. Building all rows eagerly is what froze
        // the UI for large queries (1898 items ≈ 35ms construct + a far heavier
        // gpui layout/paint pass every frame).
        let count = filter_items(
            &self.items,
            self.stream_filter.as_deref(),
            "",
            self.current_user.as_deref(),
        )
        .len();

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

        container.child(
            uniform_list(
                "items-list",
                count,
                cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                    let filtered = filter_items(
                        &this.items,
                        this.stream_filter.as_deref(),
                        "",
                        this.current_user.as_deref(),
                    );
                    let mut rows = Vec::new();
                    for ix in range {
                        let Some(item) = filtered.get(ix) else {
                            continue;
                        };
                        let mut meta = format!(
                            "{}/{}#{}  ·  {}  ·  @{}",
                            item.repo_owner,
                            item.repo_name,
                            item.number,
                            item.state,
                            item.author.as_deref().unwrap_or("ghost"),
                        );
                        if !item.labels.is_empty() {
                            meta.push_str("  ·  ");
                            meta.push_str(&item.labels.join(", "));
                        }
                        let is_new = item.is_new;
                        let title = item.title.clone();

                        rows.push(
                            v_flex()
                                .id(ix)
                                .h(px(52.))
                                .w_full()
                                .px_4()
                                .justify_center()
                                .gap_0p5()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(
                                    h_flex()
                                        .w_full()
                                        .gap_2()
                                        .items_center()
                                        .when(is_new, |e| {
                                            e.child(
                                                div()
                                                    .flex_shrink_0()
                                                    .text_xs()
                                                    .font_bold()
                                                    .text_color(cx.theme().accent_foreground)
                                                    .bg(cx.theme().accent)
                                                    .px_1p5()
                                                    .rounded_md()
                                                    .child("NEW"),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .font_bold()
                                                .text_color(cx.theme().foreground)
                                                .child(SharedString::from(title)),
                                        ),
                                )
                                .child(
                                    div()
                                        .w_full()
                                        .truncate()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(SharedString::from(meta)),
                                ),
                        );
                    }
                    rows
                }),
            )
            .h_full(),
        )
    }
}

impl Render for GlaucaApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let user = match &self.current_user {
            Some(u) => format!("connected as {u}"),
            None => "not authenticated".to_string(),
        };
        let mut status_bits = Vec::new();
        if self.syncing {
            status_bits.push("syncing…".to_string());
        }
        if self.bg_sync_pending > 0 {
            status_bits.push(format!("{} bg", self.bg_sync_pending));
        }
        if let Some(s) = &self.status {
            status_bits.push(s.clone());
        }
        let header = if status_bits.is_empty() {
            user
        } else {
            format!("{user}  ·  {}", status_bits.join("  ·  "))
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .px_4()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(SharedString::from(header)),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_left(cx))
                    .child(self.render_items(cx)),
            )
    }
}

fn main() -> Result<()> {
    // The engine runs on its own multi-thread tokio runtime; `rt` must outlive
    // the gpui event loop so its background tasks keep being driven.
    let rt = tokio::runtime::Runtime::new()?;
    let (engine, init) = rt.block_on(async {
        let db_path = db::default_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pool = db::open_pool(&db_path).await?;
        let gh = github::build_client()?;
        Engine::start(pool, gh).await
    })?;

    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(WindowOptions::default(), move |window, cx| {
                window.set_window_title("glauca");
                let view = cx.new(|cx| GlaucaApp::new(engine, init, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })
            .expect("Failed to open window");
        })
        .detach();
    });

    // Keep the runtime alive across the whole GUI lifetime.
    drop(rt);
    Ok(())
}
