//! glauca-gui — gpui front-end for glauca. The application lives in the `gui`
//! module; `main` only initializes logging and TLS, then hands off to `gui::run`.

mod gui;

use anyhow::Result;

fn main() -> Result<()> {
    // Keep the guard alive for the whole program so buffered logs are flushed on
    // exit. Logs go to a file under the data dir (shared with the TUI).
    let _log_guard = glauca_core::logging::init("glauca-gui", "glauca_core=info,glauca_gui=info");
    tracing::info!("glauca-gui starting");

    // rustls needs a process-level CryptoProvider, but with both aws-lc-rs and
    // ring in the dependency graph it can't auto-select one. Install ring before
    // any TLS use (the avatar HTTP client). Ignore the error if already set.
    let _ = rustls::crypto::ring::default_provider().install_default();

    gui::run()
}
