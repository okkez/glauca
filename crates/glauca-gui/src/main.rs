//! glauca-gui — gpui front-end for glauca (phase B, MVP 閲覧先行).
//!
//! B0: prove the plumbing. gpui owns the main-thread event loop and is not
//! tokio-aware, so the async engine runs on a separate multi-thread tokio
//! runtime. The view periodically drains `engine.try_recv()` and repaints.
//! This step only renders the authenticated user and the left-pane entry
//! labels — no selection / item list yet (that is B1).

use std::time::Duration;

use anyhow::Result;
use glauca_core::engine::{AppMessage, Engine, EngineInit};
use glauca_core::types::LeftPaneEntry;
use glauca_core::{db, github};
use gpui::*;
use gpui_component::*;
use smol::Timer;

/// How often the GUI drains engine messages and repaints.
const DRAIN_INTERVAL: Duration = Duration::from_millis(50);

/// Minimal B0 GUI state.
struct GlaucaApp {
    engine: Engine,
    entries: Vec<LeftPaneEntry>,
    current_user: Option<String>,
    status: Option<String>,
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

        let EngineInit {
            entries,
            current_user,
        } = init;
        Self {
            engine,
            entries,
            current_user,
            status: None,
        }
    }

    /// Apply a single engine message to GUI state. B0 only surfaces a few as
    /// status text; item/selection handling arrives in B1.
    fn apply(&mut self, msg: AppMessage) {
        match msg {
            AppMessage::Status(s) => self.status = Some(s),
            AppMessage::SyncStarted { .. } => self.status = Some("Syncing…".into()),
            AppMessage::SyncDone { count, .. } => {
                self.status = Some(format!("Synced {count} items"))
            }
            AppMessage::SyncError { error, .. } => self.status = Some(format!("Sync error: {error}")),
            AppMessage::BgSyncQueued(n) => self.status = Some(format!("Queued {n} background syncs")),
            _ => {}
        }
    }
}

impl Render for GlaucaApp {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = match &self.current_user {
            Some(u) => format!("connected as {u}"),
            None => "not authenticated".to_string(),
        };

        let mut col = div()
            .v_flex()
            .gap_1()
            .p_4()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(SharedString::from(header))
            .child(SharedString::from(format!(
                "{} entries{}",
                self.entries.len(),
                self.status
                    .as_ref()
                    .map(|s| format!("  ·  {s}"))
                    .unwrap_or_default()
            )));

        for entry in &self.entries {
            let label = match entry {
                LeftPaneEntry::Query(q) => q.label.clone(),
                LeftPaneEntry::FilterStream(fs) => format!("  └ {}", fs.name),
            };
            col = col.child(SharedString::from(label));
        }

        col
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
