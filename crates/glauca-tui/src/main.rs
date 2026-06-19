use glauca_core::{db, github};
mod tui;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Keep the guard alive for the whole program so buffered logs are flushed on
    // exit. The TUI owns the terminal, so logs go to a file (see logging::init).
    let _log_guard = glauca_core::logging::init("glauca-tui", "glauca_core=info,glauca_tui=info");
    tracing::info!("glauca-tui starting");

    let db_path = db::default_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pool = db::open_pool(&db_path).await?;
    let gh_client = github::build_client()?;
    tui::run(pool, gh_client).await?;
    Ok(())
}
