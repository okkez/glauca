//! Application entry point: builds the engine and gpui window, then runs the
//! event loop. Split out of `main` so `main.rs` stays thin (mirrors the TUI's
//! `tui::run`).

use anyhow::Result;
use glauca_core::db;
use glauca_core::engine::Engine;
use glauca_core::github;
use gpui::*;
use gpui_component::Root;

use super::assets;
use super::settings::GuiSettings;
use super::*;

pub(crate) fn run() -> Result<()> {
    // Load settings once; the GlaucaApp copy is the single source of truth from
    // here on (only ever written back from there).
    let settings = GuiSettings::load();

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
        Engine::start(pool, gh, settings.sync_interval_secs).await
    })?;

    gpui_platform::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            // gpui defaults to a NullHttpClient, which can't fetch the remote
            // GitHub avatar URLs the item-list avatars use (every fetch fails and
            // the Avatar falls back to its placeholder). Install a real client.
            match reqwest_client::ReqwestClient::user_agent("glauca-gui") {
                Ok(client) => cx.set_http_client(std::sync::Arc::new(client)),
                Err(e) => tracing::warn!(error = %e, "failed to init HTTP client for avatars"),
            }
            gpui_component::init(cx);

            // Navigation/edit keys are scoped to "Glauca && !Input": a bare "Glauca"
            // binding would still fire while a gpui-component Input is focused, because
            // dispatch bubbles to the root node (where the context is just [Glauca]) and
            // matches there — swallowing letters meant for the text box. The `!Input`
            // term is evaluated against the *full* focus path, so it disables these
            // bindings whenever an Input is anywhere in the chain. Escape stays plain
            // "Glauca" so it can blur the filter / close a dialog from inside the Input.
            cx.bind_keys([
                KeyBinding::new("j", MoveDown, Some(NAV_CONTEXT)),
                KeyBinding::new("k", MoveUp, Some(NAV_CONTEXT)),
                KeyBinding::new("down", MoveDown, Some(NAV_CONTEXT)),
                KeyBinding::new("up", MoveUp, Some(NAV_CONTEXT)),
                KeyBinding::new("h", FocusLeft, Some(NAV_CONTEXT)),
                KeyBinding::new("l", FocusRight, Some(NAV_CONTEXT)),
                KeyBinding::new("left", FocusLeft, Some(NAV_CONTEXT)),
                KeyBinding::new("right", FocusRight, Some(NAV_CONTEXT)),
                KeyBinding::new("enter", Activate, Some(NAV_CONTEXT)),
                KeyBinding::new("/", FocusFilter, Some(NAV_CONTEXT)),
                KeyBinding::new("escape", Cancel, Some(GLAUCA_CONTEXT)),
                KeyBinding::new("n", NewQuery, Some(NAV_CONTEXT)),
                KeyBinding::new("f", NewFilterStream, Some(NAV_CONTEXT)),
                KeyBinding::new("e", EditEntry, Some(NAV_CONTEXT)),
                KeyBinding::new("d", DeleteEntry, Some(NAV_CONTEXT)),
                KeyBinding::new("shift-j", ReorderDown, Some(NAV_CONTEXT)),
                KeyBinding::new("shift-k", ReorderUp, Some(NAV_CONTEXT)),
                KeyBinding::new("q", Quit, Some(NAV_CONTEXT)),
                KeyBinding::new("o", OpenInBrowser, Some(NAV_CONTEXT)),
                KeyBinding::new("c", OpenComments, Some(NAV_CONTEXT)),
                KeyBinding::new("y", CopyUrl, Some(NAV_CONTEXT)),
                KeyBinding::new("x", RunCustomAction, Some(NAV_CONTEXT)),
                KeyBinding::new("r", Refresh, Some(NAV_CONTEXT)),
                // Comments overlay controls (active only while the overlay is focused).
                KeyBinding::new("j", CommentsScrollDown, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("k", CommentsScrollUp, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("down", CommentsScrollDown, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("up", CommentsScrollUp, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("g", CommentsTop, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("shift-g", CommentsBottom, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("s", CommentsToggleSort, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("h", CommentsToggleHidden, Some(COMMENTS_CONTEXT)),
                KeyBinding::new("q", CommentsClose, Some(COMMENTS_CONTEXT)),
            ]);

            cx.spawn(async move |cx| {
                cx.open_window(WindowOptions::default(), move |window, cx| {
                    window.set_window_title("glauca");
                    let view = cx.new(|cx| GlaucaApp::new(engine, init, settings, window, cx));
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
