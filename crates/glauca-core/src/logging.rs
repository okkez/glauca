//! Shared `tracing` setup for the front-end binaries.
//!
//! The TUI occupies stdout (alternate screen + raw mode), so logs go to a file
//! under the user data dir rather than the terminal. The GUI uses the same file
//! sink for consistency. Files rotate daily and only the most recent few days
//! are kept, so the log never grows without bound.

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{Builder, Rotation};
use tracing_subscriber::EnvFilter;

/// How many daily log files to keep; older ones are removed automatically.
const MAX_LOG_FILES: usize = 7;

/// Initialize file-based `tracing` for a binary and return a flush guard.
///
/// `prefix` names the log file (e.g. `"glauca-tui"` → `glauca-tui.<date>.log`).
/// `default_filter` is the `EnvFilter` directive used when `RUST_LOG` is unset
/// (each binary passes its own crates, so core doesn't need to know them).
/// The caller MUST keep the returned guard alive for the whole program: it owns
/// the non-blocking writer's worker thread and flushes buffered logs on drop.
/// Returns `None` (logging disabled, best-effort) if the data dir can't be
/// resolved or the appender can't be built.
pub fn init(prefix: &str, default_filter: &str) -> Option<WorkerGuard> {
    let dir = dirs::data_local_dir()?.join("glauca");

    let appender = Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .filename_suffix("log")
        .max_log_files(MAX_LOG_FILES)
        .build(&dir)
        .ok()?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    // `try_init` rather than `init`: never panic if a subscriber is already set
    // (e.g. a future test that also initializes tracing).
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .try_init();

    Some(guard)
}
