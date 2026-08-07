//! ESPHome daemon process management
//!
//! Handles starting, stopping, and monitoring the ESPHome dashboard process.
//!
//! [`DaemonManager`] and the state it guards live here, together with the
//! tray-emitting `start`/`stop` wrappers and the loopback probe. The two
//! sequences those wrappers delegate to are split by lifecycle phase:
//! `spawn` brings the child up and installs its watchers, `shutdown` takes it
//! back down. Both are `impl DaemonManager` blocks, so the type reads as one
//! from every call site.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tokio::process::Child;
use tokio::sync::Mutex;

use crate::platform;
use crate::settings::Settings;

mod shutdown;
mod spawn;

/// Width-correct atomic and integer types for the dashboard child PID.
/// Windows PIDs are a `DWORD` (`u32`); Unix PIDs are a `pid_t` (`i32`).
/// Matching the native width lets `child.id()` round-trip losslessly on both:
/// a Windows `u32` PID above `i32::MAX` would otherwise wrap negative when
/// forced through an `i32` and the shutdown path would target the wrong
/// process.
#[cfg(windows)]
type AtomicPid = std::sync::atomic::AtomicU32;
#[cfg(unix)]
type AtomicPid = std::sync::atomic::AtomicI32;
#[cfg(windows)]
type PidInt = u32;
#[cfg(unix)]
type PidInt = i32;

/// Human-readable name of the backend process, for log messages.
const BACKEND_NAME: &str = "ESPHome device builder";

/// File name of the backend's combined stdout+stderr log inside the logs
/// directory. The CLI's `logs` subcommand tails this file by name, so the
/// daemon and the client must agree on it.
pub(crate) const DASHBOARD_LOG_NAME: &str = "dashboard.log";

/// Number of previous `dashboard.log` runs to retain when rotating on start
/// (`dashboard.log.1` … `dashboard.log.3`). Enough to inspect the run that
/// preceded a failed restart without unbounded disk growth.
const LOG_HISTORY: usize = 3;

/// Manages the ESPHome Device Builder process
pub struct DaemonManager {
    /// The running process, if any
    process: Arc<Mutex<Option<Child>>>,
    /// Path to bundled Python executable
    python_path: PathBuf,
    /// Path to bundled Python bin directory (for PATH)
    python_bin_dir: PathBuf,
    /// Path to config directory
    config_dir: PathBuf,
    /// Path to logs directory
    logs_dir: PathBuf,
    /// Dashboard port
    port: u16,
    /// Whether the daemon is running
    running: Arc<AtomicBool>,
    /// PID of the device builder child, mirrored as an atomic so synchronous
    /// exit paths (e.g. macOS Dock-Quit, which fires `RunEvent::Exit`
    /// without going through `ExitRequested`) can SIGTERM the process
    /// group without locking the tokio mutex. Zero when no child is
    /// running.
    dashboard_pid: Arc<AtomicPid>,
    /// AppHandle for emitting notifications / updating the tray when the
    /// child process exits independently of an explicit `stop()`. Also used
    /// to read the desktop app version (forwarded to the backend via
    /// `ESPHOME_DESKTOP_VERSION` at `start()` time).
    app_handle: AppHandle,
}

impl DaemonManager {
    /// Create a new daemon manager
    pub fn new(app_handle: &AppHandle, settings: &Settings) -> Result<Self> {
        let data_dir = platform::get_data_dir(app_handle)?;
        let python_path = platform::get_python_path(app_handle)?;
        let python_bin_dir = platform::get_python_bin(app_handle)?;

        // Use ~/esphome as the default config directory
        let config_dir = settings
            .config_dir
            .clone()
            .unwrap_or_else(crate::settings::default_config_dir);
        std::fs::create_dir_all(&config_dir).context("Failed to create config directory")?;

        // Create logs directory in app data
        let logs_dir = data_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).context("Failed to create logs directory")?;

        Ok(Self {
            process: Arc::new(Mutex::new(None)),
            python_path,
            python_bin_dir,
            config_dir,
            logs_dir,
            port: settings.port,
            running: Arc::new(AtomicBool::new(false)),
            dashboard_pid: Arc::new(AtomicPid::new(0)),
            app_handle: app_handle.clone(),
        })
    }

    /// Start the ESPHome device builder.
    ///
    /// Emits the tray status itself — "Running" on success, "Stopped" on
    /// failure — so callers don't have to pair every start with a
    /// `tray::update_status` call (a forgotten pairing leaves the tray
    /// stale). The status reflects the actual post-call state, not the
    /// intent.
    pub async fn start(&self) -> Result<()> {
        let result = self.start_inner().await;
        crate::tray::update_status(&self.app_handle, self.is_running());
        result
    }

    /// Stop the ESPHome dashboard.
    ///
    /// Emits the tray status itself: "Stopped" optimistically up front (the
    /// graceful drain in `stop_inner` can take up to 30s, during which the
    /// tray should not claim the backend is running), restored to the actual
    /// state if the stop fails — after a failed stop the backend may well
    /// still be running, so the optimistic label must not stand.
    ///
    /// Returns `Err` when the stop could not be confirmed: on Unix, if the
    /// backend ignores SIGTERM for the full 30s drain window (we never
    /// escalate to SIGKILL by design), the process is left running and this
    /// reports the failure so callers can abort rather than act as if the
    /// backend were down.
    pub async fn stop(&self) -> Result<()> {
        crate::tray::update_status(&self.app_handle, false);
        let result = self.stop_inner().await;
        if result.is_err() {
            crate::tray::update_status(&self.app_handle, self.is_running());
        }
        result
    }

    /// Restart the daemon
    pub async fn restart(&self) -> Result<()> {
        self.stop().await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        self.start().await
    }

    /// Check if the daemon is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get the port the daemon is running on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the config directory
    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }

    /// Get the logs directory
    pub fn logs_dir(&self) -> &PathBuf {
        &self.logs_dir
    }
}

/// Build the loopback URL used to probe the dashboard (both the startup
/// readiness poll and the periodic health check).
///
/// The backend is spawned with `--address 127.0.0.1` / `--host 127.0.0.1`
/// (see `DaemonManager::start()`), so it only listens on the IPv4 loopback.
/// Probing the literal `127.0.0.1` rather than the `localhost` hostname
/// avoids a resolver detour: on IPv6-first hosts `localhost` resolves to
/// `::1` first, where nothing is listening, producing spurious probe
/// failures (and a connect stall per attempt before the IPv4 fallback).
pub(crate) fn loopback_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/", port)
}

/// Perform a health check on the dashboard. Also used by the control
/// server's `status` reply.
pub(crate) async fn health_check(port: u16) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let url = loopback_url(port);
    match client.get(&url).send().await {
        Ok(response) => Ok(response.status().is_success()),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::loopback_url;

    #[test]
    fn loopback_url_targets_ipv4_loopback() {
        // Must match the address the backend binds (`127.0.0.1`), not the
        // `localhost` hostname, so the probe doesn't get steered to `::1`
        // on IPv6-first hosts where the daemon isn't listening.
        let url = loopback_url(6052);
        assert_eq!(url, "http://127.0.0.1:6052/");
        assert!(!url.contains("localhost"));
    }
}
