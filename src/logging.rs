use std::{fs, io, panic};

use tracing::{Event, Subscriber, field::Visit};
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    EnvFilter,
    layer::{Context, Filter, Layer, SubscriberExt},
    util::SubscriberInitExt,
};

use crate::platform;

// These dependency warnings describe expected desktop/filesystem races that Dirigent
// cannot act on: optional D-Bus services may be absent, and inotify may report that a
// deleted child watch was already removed by the kernel.
const DEFAULT_FILTER: &str = "warn,dirigent=info,zbus::proxy=error,notify::inotify=error";

#[derive(Clone, Copy)]
struct ExpectedNoiseFilter;

impl<S: Subscriber> Filter<S> for ExpectedNoiseFilter {
    fn enabled(&self, _: &tracing::Metadata<'_>, _: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &Event<'_>, _: &Context<'_, S>) -> bool {
        let mut message = EventMessage::default();
        event.record(&mut message);
        !is_expected_noise(event.metadata().target(), message.value.as_deref())
    }
}

#[derive(Default)]
struct EventMessage {
    value: Option<String>,
}

impl Visit for EventMessage {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.value = Some(value.to_string());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.value = Some(format!("{value:?}"));
        }
    }
}

fn is_expected_noise(target: &str, message: Option<&str>) -> bool {
    target == "gpui::window"
        && message.is_some_and(|message| message.trim_matches('"') == "window not found")
}

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
                .with_thread_names(true)
                .with_filter(ExpectedNoiseFilter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_thread_names(true)
                .with_filter(ExpectedNoiseFilter),
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
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_thread_names(true)
                .with_filter(ExpectedNoiseFilter),
        )
        .with(env_filter())
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
