//! Tracing setup: stderr plus a rolling app-level log file.

use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// Number of rotated app-log files to retain (one per day of activity).
const APP_LOG_HISTORY: usize = 7;

/// Build the rolling app-level log appender (`<data>/logs/app.<date>.log`).
///
/// Resolved without an `AppHandle` (logging is initialised before Tauri builds
/// one) using the same bundle identifier Tauri's `app_data_dir()` uses, so this
/// sits next to the dashboard logs and stays inspectable across a self-update
/// restart — issue #203. Daily rotation with [`APP_LOG_HISTORY`] retained keeps
/// it bounded even when the filter is raised to `debug` to chase a failure.
/// Best-effort: returns None if the dir or appender can't be built, leaving
/// stderr logging.
fn app_log_appender() -> Option<tracing_appender::rolling::RollingFileAppender> {
    let dir = crate::platform::data_dir_no_handle()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("app")
        .filename_suffix("log")
        .max_log_files(APP_LOG_HISTORY)
        .build(dir)
        .ok()
}

/// Initialize logging
pub(crate) fn init_logging() {
    // Optional rolling file layer beside stderr: a no-op when the appender can't
    // be built, so a path failure never blocks startup. The appender is its own
    // `MakeWriter`, so there's no per-event handle clone and no panic path in
    // the logging hot loop.
    let file_layer = app_log_appender().map(|appender| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(appender)
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "esphome_desktop=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
        .init();
}
