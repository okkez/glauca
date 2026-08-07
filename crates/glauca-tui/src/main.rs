use clap::Parser;
use glauca_core::{db, github};
use std::path::PathBuf;
mod tui;

/// Terminal UI for browsing and triaging GitHub issues and pull requests.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the cache database. Takes precedence over the GLAUCA_DB_PATH
    /// environment variable; both default to <data dir>/glauca/cache.db.
    #[arg(long, value_name = "PATH")]
    db_path: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse args first so `--version`/`--help` print and exit before we touch the log dir,
    // DB, or TLS provider — which also keeps those flags usable without a TTY.
    let cli = Cli::parse();

    // Keep the guard alive for the whole program so buffered logs are flushed on exit.
    let _log_guard = glauca_core::logging::init("glauca-tui", "glauca_core=info,glauca_tui=info");
    tracing::info!("glauca-tui starting");

    // rustls needs a process-level CryptoProvider, and with both aws-lc-rs and ring in the
    // graph it can't auto-select. Install ring before any TLS use.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let pool = db::open_pool(&db::resolve_db_path(cli.db_path)).await?;
    let gh_client = github::build_client()?;
    tui::run(pool, gh_client).await?;
    Ok(())
}
