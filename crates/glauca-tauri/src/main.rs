//! glauca-tauri — a web-tech (HTML/CSS/JS) front-end for glauca built on Tauri.
//!
//! Like `glauca-tui` and `glauca-gui`, this is a thin shell over the shared
//! `glauca-core` engine. The wiring has two halves:
//!
//!   * front-end → engine: JavaScript calls `invoke('<command>', …)`, handled by
//!     the `#[tauri::command]` functions in [`commands`], which forward an
//!     [`EngineCommand`] on the engine's channel.
//!   * engine → front-end: a background task drains `engine.recv()` and emits each
//!     `AppMessage` as the `app-message` Tauri event, which the front-end listens
//!     for and folds into its UI state.
//!
//! The engine is started before the Tauri event loop (via the Tauri-managed async
//! runtime). `glauca-core` owns all DB / network / process work, so this crate has
//! no business logic of its own.

mod commands;
mod settings;

use commands::AppState;
use glauca_core::engine::Engine;
use glauca_core::{db, github};
use tauri::Emitter;

fn main() -> anyhow::Result<()> {
    let _log_guard =
        glauca_core::logging::init("glauca-tauri", "glauca_core=info,glauca_tauri=info");
    tracing::info!("glauca-tauri starting");

    // rustls needs a process-level CryptoProvider; with both aws-lc-rs and ring in
    // the graph it can't auto-select. Install ring before any TLS use (mirrors the
    // TUI/GUI front-ends). Ignore the error if already set.
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Honor the user's configured sync interval (same per-front-end TOML pattern
    // as glauca-tui / glauca-gui), falling back to the shared core default.
    let sync_interval_secs = settings::TauriSettings::load().sync_interval_secs;

    // Bring up DB + GitHub client + engine on the Tauri-managed tokio runtime, so
    // the engine's internal `tokio::spawn` tasks share that runtime with the async
    // command handlers below.
    let (engine, init_json, current_user) = tauri::async_runtime::block_on(async {
        let db_path = db::default_db_path();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let pool = db::open_pool(&db_path).await?;
        let gh_client = github::build_client()?;
        let (engine, init) = Engine::start(pool, gh_client, sync_interval_secs).await?;
        let current_user = init.current_user.clone();
        let init_json = serde_json::to_value(&init)?;
        anyhow::Ok((engine, init_json, current_user))
    })?;

    let sender = engine.sender();

    tauri::Builder::default()
        .manage(AppState {
            tx: sender,
            init: init_json,
            current_user,
        })
        .setup(move |app| {
            // Stream engine messages to the front-end. `emit` requires the payload
            // to be `Clone`, which `AppMessage` is not, so serialize to a JSON
            // value (Clone + Serialize) first.
            let handle = app.handle().clone();
            let mut engine = engine;
            tauri::async_runtime::spawn(async move {
                while let Some(msg) = engine.recv().await {
                    match serde_json::to_value(&msg) {
                        Ok(value) => {
                            if let Err(e) = handle.emit("app-message", value) {
                                tracing::warn!(error = %e, "failed to emit app-message");
                            }
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to serialize AppMessage"),
                    }
                }
                tracing::info!("engine message stream ended");
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::init,
            commands::unread_counts,
            commands::filter_items,
            commands::load_cached,
            commands::sync,
            commands::full_resync,
            commands::sync_if_stale,
            commands::refresh_item,
            commands::mark_entry_viewed,
            commands::enqueue_stale,
            commands::add_query,
            commands::add_filter_stream,
            commands::edit_query,
            commands::edit_filter_stream,
            commands::delete_query,
            commands::delete_filter_stream,
            commands::swap_query_positions,
            commands::swap_filter_stream_positions,
            commands::load_comments,
            commands::open_browser,
            commands::comment,
            commands::submit_review,
            commands::merge,
            commands::mark_item_read,
            commands::mark_all_read,
        ])
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("tauri run error: {e}"))?;

    Ok(())
}
