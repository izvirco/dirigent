//! Logging for both binaries: `tracing` events go to stderr and to a daily file in
//! `<data dir>/logs/` (`<name>.YYYY-MM-DD.log`, 30 days of them kept; `<name>.log` links
//! today's, except on Windows). Times are UTC, as the files roll at UTC midnight.
//!
//! By default dirigent's crates log at info and their dependencies (gpui's `log` records
//! included) only warn. `DIRIGENT_LOG` adds directives on top, in `RUST_LOG` syntax:
//! `dirigent_service=debug`, `dirigent=debug` (a target matches by prefix: both crates),
//! `zbus=error`.

use std::io::IsTerminal;
use tracing_appender::rolling::{self, Rotation};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, fmt};

const DEFAULT_FILTER: &str = "warn,dirigent=info";
const KEEP_FILES: usize = 30;

/// Start logging as `name` (`desktop`, `service`: the file's name). Call it right after
/// `init_dirs`, which says where the data dir is. Panics are logged from then on, so a
/// crash leaves its reason in the file.
pub fn init_logging(name: &str) {
    // Added to the defaults rather than replacing them: a lone `dirigent_service=debug`
    // would otherwise silence every other target, panics (`dirigent_service::logging`) too.
    let filter = EnvFilter::new(format!("{DEFAULT_FILTER},{}", std::env::var("DIRIGENT_LOG").unwrap_or_default()));
    let stderr = fmt::layer().with_writer(std::io::stderr).with_ansi(std::io::stderr().is_terminal());
    // Written in place: the volume is low, and nothing is lost when the process exits.
    // The directory made first: the appender prunes old files before it makes it.
    let dir = crate::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let file = rolling::Builder::new().rotation(Rotation::DAILY).filename_prefix(name).filename_suffix("log").max_log_files(KEEP_FILES);
    // Not on Windows: a symlink needs developer mode there, and a failed one fails the file.
    #[cfg(unix)]
    let file = file.latest_symlink(format!("{name}.log"));
    let file = file.build(dir);
    let (file, file_error) = match file {
        Ok(f) => (Some(fmt::layer().with_ansi(false).with_writer(f)), None),
        Err(e) => (None, Some(e)),
    };
    if let Err(e) = tracing_subscriber::registry().with(filter).with(stderr).with(file).try_init() {
        // Nothing to log through: a logger was already installed.
        eprintln!("logging not set up: {e}");
        return;
    }
    if let Some(e) = file_error {
        tracing::warn!("logging to stderr only: {e}");
    }
    // Instead of the default hook's print, which only reaches stderr.
    std::panic::set_hook(Box::new(|info| {
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::capture();
        let backtrace = match backtrace.status() {
            std::backtrace::BacktraceStatus::Captured => format!("\n{backtrace}"),
            _ => String::new(),
        };
        tracing::error!("thread '{}' {info}{backtrace}", thread.name().unwrap_or("<unnamed>"));
    }));
}
