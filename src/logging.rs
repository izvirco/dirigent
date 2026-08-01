use std::{fs, io, panic};

use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::platform;

const DEFAULT_FILTER: &str = "warn,dirigent=info";

pub(crate) struct LoggingGuard {
    _file_guard: WorkerGuard,
}

pub(crate) fn initialize() -> Result<LoggingGuard, String> {
    let directory = platform::logs_directory()?;
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "could not create log directory {}: {error}",
            directory.display()
        )
    })?;

    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("dirigent")
        .filename_suffix("log")
        .build(&directory)
        .map_err(|error| {
            format!(
                "could not initialize file logging in {}: {error}",
                directory.display()
            )
        })?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_thread_names(true),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_thread_names(true),
        )
        .with(env_filter())
        .try_init()
        .map_err(|error| format!("could not install tracing subscriber: {error}"))?;

    install_panic_hook();
    tracing::info!(log_directory = %directory.display(), "logging initialized");
    Ok(LoggingGuard {
        _file_guard: file_guard,
    })
}

pub(crate) fn initialize_console() {
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_thread_names(true)
        .with_env_filter(env_filter())
        .try_init();
    install_panic_hook();
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

fn install_panic_hook() {
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        tracing::error!(panic = %panic_info, "application panicked");
        previous_hook(panic_info);
    }));
}

#[cfg(test)]
mod tests {
    #[test]
    fn stores_logs_beside_v0_state() {
        assert!(
            crate::platform::logs_directory()
                .unwrap()
                .ends_with("dirigent/v0/logs")
        );
    }
}
