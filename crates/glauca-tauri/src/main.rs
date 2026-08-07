//! glauca-tauri — a web-tech (HTML/CSS/JS) front-end for glauca built on Tauri.
//!
//! The wiring has two halves:
//!
//!   * front-end → engine: JavaScript calls `invoke('<command>', …)`, handled by the
//!     `#[tauri::command]` functions in [`commands`].
//!   * engine → front-end: a background task drains `engine.recv()` and emits each
//!     `AppMessage` as the `app-message` Tauri event.
//!
//! The engine is started before the Tauri event loop, on the Tauri-managed async runtime,
//! so its spawned tasks share that runtime with the async command handlers.

mod commands;
mod settings;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use clap::Parser;
use commands::AppState;
use glauca_core::engine::{AppMessage, Engine};
use glauca_core::notify::{ItemTracker, notify_updated_items};
use glauca_core::{db, github};
use tauri::Emitter;

/// Desktop web-tech (Tauri) UI for browsing and triaging GitHub issues and pull requests.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the cache database. Takes precedence over the GLAUCA_DB_PATH environment
    /// variable; both default to <data dir>/glauca/cache.db.
    ///
    /// Via `cargo tauri dev` the flag needs two separators to get past both cargo and the
    /// Tauri CLI: `cargo tauri dev -- -- --db-path PATH`.
    #[arg(long, value_name = "PATH")]
    db_path: Option<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    // Parse args first so `--version`/`--help` print and exit before we touch the log dir,
    // DB, or TLS provider.
    let cli = Cli::parse();

    let _log_guard =
        glauca_core::logging::init("glauca-tauri", "glauca_core=info,glauca_tauri=info");
    tracing::info!("glauca-tauri starting");

    // rustls needs a process-level CryptoProvider, and with both aws-lc-rs and ring in the
    // graph it can't auto-select. Install ring before any TLS use.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let settings = settings::TauriSettings::load();
    let sync = glauca_core::engine::SyncConfig::effective(
        settings.sync_interval_secs,
        settings.full_fetch_interval_secs,
    );
    let maintenance = glauca_core::engine::MaintenanceConfig::effective(
        settings.retention_days,
        settings.max_items_per_query,
    );

    let (engine, init_entries, current_user, pool, query_names) =
        tauri::async_runtime::block_on(async {
            let pool = db::open_pool(&db::resolve_db_path(cli.db_path)).await?;
            let gh_client = github::build_client()?;
            // A clone for AppState; the engine takes ownership of the original.
            let pool_for_state = pool.clone();
            let (engine, init) = Engine::start(pool, gh_client, sync, maintenance).await?;
            let current_user = commands::CurrentUserState {
                login: init.current_user,
                name: init.current_user_name,
                avatar_url: init.current_user_avatar_url,
            };
            let query_names = commands::query_name_map(&init.entries);
            anyhow::Ok((
                engine,
                init.entries,
                current_user,
                pool_for_state,
                query_names,
            ))
        })?;

    let sender = engine.sender();
    let notifications_enabled = Arc::new(AtomicBool::new(settings.notifications_enabled));
    let query_names = Arc::new(Mutex::new(query_names));
    let current_user = Arc::new(RwLock::new(current_user));

    // Clones for the engine-message loop in setup().
    let notif_loop = notifications_enabled.clone();
    let tracker_loop = Arc::new(Mutex::new(ItemTracker::new()));
    let names_loop = query_names.clone();
    let current_user_loop = current_user.clone();

    tauri::Builder::default()
        .manage(AppState {
            tx: sender,
            init_entries,
            current_user,
            pool,
            notifications_enabled,
            query_names,
            // Loaded once: edits to actions.toml take effect on the next launch.
            custom_actions: glauca_core::actions::CustomActions::load(),
        })
        .setup(move |app| {
            // `emit` requires a `Clone` payload, which `AppMessage` is not, so each
            // message is serialized to a JSON value first.
            let handle = app.handle().clone();
            let mut engine = engine;
            tauri::async_runtime::spawn(async move {
                while let Some(msg) = engine.recv().await {
                    // The tracker's baseline is maintained even when notifications are
                    // disabled, so toggling on mid-session doesn't re-announce everything.
                    if let AppMessage::ItemsLoaded {
                        query_id,
                        items,
                        background,
                    } = &msg
                    {
                        let enabled = notif_loop.load(Ordering::Relaxed);
                        // Recover from poisoning: these locks guard plain map/tracker
                        // state, consistent even if a panicking thread abandoned it.
                        let to_notify = tracker_loop
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .changed_count_to_notify(*query_id, items, *background, enabled);
                        if let Some(n) = to_notify {
                            let name = names_loop
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .get(query_id)
                                .cloned()
                                .unwrap_or_else(|| format!("Query #{query_id}"));
                            // notify_updated_items is a blocking D-Bus call on Linux.
                            tauri::async_runtime::spawn_blocking(move || {
                                notify_updated_items(&name, n)
                            });
                        }
                    }
                    // Adopt a login the engine resolved after the startup lookup failed.
                    // Written before the message reaches JS, so the re-filter the
                    // front-end runs on it already sees the new login.
                    if let AppMessage::CurrentUserResolved {
                        login,
                        name,
                        avatar_url,
                    } = &msg
                    {
                        *current_user_loop
                            .write()
                            .unwrap_or_else(std::sync::PoisonError::into_inner) =
                            commands::CurrentUserState {
                                login: Some(login.clone()),
                                name: name.clone(),
                                avatar_url: avatar_url.clone(),
                            };
                    }
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
            commands::quit,
            commands::get_settings,
            commands::save_settings,
            commands::list_entries,
            commands::unread_counts,
            commands::filter_items,
            commands::count_item_changes,
            commands::load_cached,
            commands::sync,
            commands::full_resync,
            commands::sync_if_stale,
            commands::refresh_item,
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
            commands::list_custom_actions,
            commands::run_custom_action,
        ])
        .run(tauri::generate_context!())
        .map_err(|e| anyhow::anyhow!("tauri run error: {e}"))?;

    Ok(())
}
