//! Configures rolling diagnostics and panic reporting.

use std::{
    backtrace::Backtrace,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    panic,
    path::Path,
    sync::Mutex,
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

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
const CRASH_LOG_FILE_NAME: &str = "dirigent-crash.log";

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
    let crash_log_path = directory.join(CRASH_LOG_FILE_NAME);
    let (crash_file, crash_file_error) = match open_crash_file(&directory) {
        Ok(file) => (Some(file), None),
        Err(error) => (None, Some(error)),
    };

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

    install_panic_hook(crash_file);
    if let Some(error) = crash_file_error {
        tracing::warn!(
            path = %crash_log_path.display(),
            %error,
            "synchronous crash logging is unavailable"
        );
    }
    tracing::info!(
        log_directory = %directory.display(),
        crash_log = %crash_log_path.display(),
        "logging initialized"
    );
    Ok(LoggingGuard {
        _file_guard: file_guard,
    })
}

pub(crate) fn initialize_console() {
    let crash_file = platform::logs_directory().ok().and_then(|directory| {
        fs::create_dir_all(&directory).ok()?;
        open_crash_file(&directory).ok()
    });
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(io::stderr)
                .with_thread_names(true)
                .with_filter(ExpectedNoiseFilter),
        )
        .with(env_filter())
        .try_init();
    install_panic_hook(crash_file);
}

fn env_filter() -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER))
}

fn open_crash_file(directory: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join(CRASH_LOG_FILE_NAME))
}

fn install_panic_hook(crash_file: Option<File>) {
    let crash_file = crash_file.map(Mutex::new);
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        if let Some(crash_file) = crash_file.as_ref() {
            write_panic_report(crash_file, panic_info);
        }
        tracing::error!(panic = %panic_info, "application panicked");
        previous_hook(panic_info);
    }));
}

fn write_panic_report(crash_file: &Mutex<File>, panic_info: &panic::PanicHookInfo<'_>) {
    let timestamp_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis());
    let thread = thread::current();
    let mut file = match crash_file.lock() {
        Ok(file) => file,
        Err(poisoned) => poisoned.into_inner(),
    };
    let _ = writeln!(
        file,
        "timestamp_unix_ms={timestamp_unix_ms} thread={:?} thread_id={:?} {panic_info}",
        thread.name().unwrap_or("<unnamed>"),
        thread.id(),
    );
    let _ = file.flush();
    let _ = file.sync_data();

    let backtrace = Backtrace::force_capture();
    let _ = writeln!(file, "backtrace:\n{backtrace}\n");
    let _ = file.flush();
    let _ = file.sync_data();
}
