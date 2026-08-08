//! glauca-gui — gpui front-end for glauca. The application lives in the `gui`
//! module; `main` only initializes logging and TLS, then hands off to `gui::run`.

mod gui;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

/// Desktop GUI for browsing and triaging GitHub issues and pull requests.
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Path to the cache database. Takes precedence over the GLAUCA_DB_PATH
    /// environment variable; both default to <data dir>/glauca/cache.db.
    #[arg(long, value_name = "PATH")]
    db_path: Option<PathBuf>,
}

fn main() -> Result<()> {
    // Parse args first so `--version`/`--help` print and exit before we touch the log dir,
    // DB, or TLS provider — and before gpui wants a display, so both work headless.
    let cli = Cli::parse();

    // Keep the guard alive for the whole program so buffered logs are flushed on exit.
    let _log_guard = glauca_core::logging::init("glauca-gui", "glauca_core=info,glauca_gui=info");
    tracing::info!("glauca-gui starting");

    // rustls needs a process-level CryptoProvider, and with both aws-lc-rs and ring in the
    // graph it can't auto-select. Install ring before any TLS use.
    let _ = rustls::crypto::ring::default_provider().install_default();

    gui::run(cli.db_path)
}
