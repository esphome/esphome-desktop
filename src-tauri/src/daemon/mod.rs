//! ESPHome daemon process management
//!
//! Handles starting, stopping, and monitoring the ESPHome dashboard process.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::platform;
use crate::settings::Settings;

/// Manages the ESPHome dashboard process
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
    /// PID of the dashboard child, mirrored as an atomic so synchronous
    /// exit paths (e.g. macOS Dock-Quit, which fires `RunEvent::Exit`
    /// without going through `ExitRequested`) can SIGTERM the process
    /// group without locking the tokio mutex. Zero when no child is
    /// running.
    dashboard_pid: Arc<AtomicI32>,
    /// Use `esphome-device-builder` instead of `esphome dashboard`
    use_device_builder: Arc<AtomicBool>,
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
        let config_dir = settings.config_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("esphome"))
                .unwrap_or_else(|| PathBuf::from("esphome"))
        });
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
            dashboard_pid: Arc::new(AtomicI32::new(0)),
            use_device_builder: Arc::new(AtomicBool::new(settings.backend.is_builder())),
            app_handle: app_handle.clone(),
        })
    }

    /// Update the device-builder flag. Takes effect on the next daemon start.
    pub fn set_use_device_builder(&self, value: bool) {
        self.use_device_builder.store(value, Ordering::SeqCst);
    }

    /// Human-readable name of the current backend, for log messages.
    fn backend_name(&self) -> &'static str {
        if self.use_device_builder.load(Ordering::SeqCst) {
            "ESPHome device builder"
        } else {
            "ESPHome dashboard"
        }
    }

    /// Start the ESPHome dashboard
    pub async fn start(&self) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            info!("Daemon already running");
            return Ok(());
        }

        let use_device_builder = self.use_device_builder.load(Ordering::SeqCst);
        let backend_name = self.backend_name();
        info!("Starting {} on port {}", backend_name, self.port);
        debug!("Python path: {:?}", self.python_path);
        debug!("Python bin: {:?}", self.python_bin_dir);
        debug!("Config dir: {:?}", self.config_dir);
        debug!("Logs dir: {:?}", self.logs_dir);

        // Verify Python exists
        if !self.python_path.exists() {
            anyhow::bail!("Python not found at {:?}", self.python_path);
        }

        // Open log file for stdout and stderr combined
        let log_path = self.logs_dir.join("dashboard.log");
        let log_file = File::create(&log_path).context("Failed to create log file")?;
        let log_file_clone = log_file.try_clone().context("Failed to clone log file handle")?;

        info!("{} logs: {:?}", backend_name, log_path);

        let config_arg = self.config_dir.to_str().unwrap_or(".");
        let port_arg = self.port.to_string();

        // Build the command
        let mut cmd = Command::new(&self.python_path);
        if use_device_builder {
            cmd.args([
                "-m",
                "esphome_device_builder",
                config_arg,
                "--host",
                "127.0.0.1",
                "--port",
                &port_arg,
            ]);
        } else {
            cmd.args([
                "-m",
                "esphome",
                "dashboard",
                config_arg,
                "--address",
                "127.0.0.1",
                "--port",
                &port_arg,
            ]);
        }
        cmd
            // Set working directory to config dir (required for PlatformIO)
            .current_dir(&self.config_dir)
            // Redirect stdout/stderr to single log file
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_clone));
        // On Unix, intentionally NOT setting `kill_on_drop(true)`. That
        // would have tokio send SIGKILL to the Child when it gets
        // dropped (either when stop()'s wait times out, or when
        // AppState drops at process teardown), which force-kills the
        // dashboard and corrupts its state. Our Unix shutdown is
        // SIGTERM only — see `stop()` and `terminate_blocking()`.
        //
        // On Windows the graceful signal is CTRL_BREAK_EVENT (see `stop()`
        // and `terminate_blocking()`), with TerminateProcess as the hard
        // fallback. Keep `kill_on_drop(true)` as a last-ditch drop-time net
        // for any path that drops the Child without going through those
        // (note it does NOT fire on the normal quit path, which calls
        // `std::process::exit()` and skips Drop).
        #[cfg(windows)]
        cmd.kill_on_drop(true);

        // Create new process group on Unix so we can kill all children
        #[cfg(unix)]
        cmd.process_group(0);

        // Prevent a console window from staying open on Windows, and put the
        // child in its own process group so we can later deliver a graceful
        // CTRL_BREAK_EVENT to it on shutdown (see daemon stop/terminate).
        platform::configure_daemon_tokio_command(&mut cmd);

        // Set environment variables
        cmd.env("ESPHOME_DASHBOARD", "1");
        // Surface the desktop app version to the backend so it can be shown
        // in the frontend (e.g. an "About" page). Set unconditionally — both
        // backends get it; classic dashboard can ignore it.
        cmd.env(
            "ESPHOME_DESKTOP_VERSION",
            self.app_handle.package_info().version.to_string(),
        );

        // On Windows, force the spawned Python (and any subprocesses it
        // spawns for compile/logs) to use UTF-8 for stdin/stdout/stderr.
        // Without this, Python falls back to the locale codec (cp1252 on
        // Western installs) when stdout is a redirected pipe — which the
        // dashboard always is — and any non-ASCII output (e.g. the wifi
        // signal-bar block characters U+2582..U+2588) raises
        // UnicodeEncodeError and drops the device's log connection.
        #[cfg(target_os = "windows")]
        cmd.env("PYTHONIOENCODING", "utf-8");

        let child = cmd.spawn().context("Failed to spawn ESPHome process")?;
        if let Some(pid) = child.id() {
            self.dashboard_pid.store(pid as i32, Ordering::SeqCst);
        }

        let mut process = self.process.lock().await;
        *process = Some(child);
        self.running.store(true, Ordering::SeqCst);

        // Start health check task
        let running = self.running.clone();
        let port = self.port;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                match health_check(port).await {
                    Ok(true) => debug!("Health check passed"),
                    Ok(false) => warn!("Health check failed - backend may be starting"),
                    Err(e) => warn!("Health check error: {}", e),
                }
            }
        });

        // Start exit watcher. Polls `child.try_wait()` so an unexpected
        // exit (e.g. the dashboard process dying on startup because of a
        // missing module) flips the running flag back to false instead
        // of leaving the tray stuck on "Status: Running". Exits cleanly
        // when `stop()` clears the running flag.
        //
        // Captures the child's PID at spawn time and exits as soon as
        // `dashboard_pid` no longer matches. Without this, a stop()/start()
        // pair faster than the 500 ms poll interval would let an old
        // watcher wake up to a new child (running=true again, fresh PID,
        // possibly different backend) and start reporting on it with the
        // stale `backend_label` and log path it captured at its own start.
        let watcher_pid = self.dashboard_pid.load(Ordering::SeqCst);
        let process = self.process.clone();
        let running = self.running.clone();
        let dashboard_pid = self.dashboard_pid.clone();
        let app_handle = self.app_handle.clone();
        let log_path_for_watcher = log_path.clone();
        let backend_label = backend_name.to_string();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                if !running.load(Ordering::SeqCst) {
                    // stop() already cleaned up.
                    return;
                }
                if dashboard_pid.load(Ordering::SeqCst) != watcher_pid {
                    // The child this watcher was created for has been
                    // replaced by a newer start(); the new spawn has its
                    // own watcher.
                    return;
                }
                let mut guard = process.lock().await;
                // Re-check under the lock: stop() takes the child and
                // resets the PID without holding the lock for the entire
                // window, so a stale watcher could otherwise still race in.
                if dashboard_pid.load(Ordering::SeqCst) != watcher_pid {
                    return;
                }
                let exited = match guard.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(status),
                        Ok(None) => None,
                        Err(e) => {
                            warn!("try_wait on {} failed: {}", backend_label, e);
                            None
                        }
                    },
                    // stop() took the child out from under us
                    None => return,
                };

                let Some(status) = exited else { continue };

                error!(
                    "{} exited unexpectedly with status: {}. See log at {:?}.",
                    backend_label, status, log_path_for_watcher
                );
                *guard = None;
                drop(guard);
                running.store(false, Ordering::SeqCst);
                dashboard_pid.store(0, Ordering::SeqCst);

                crate::tray::update_status(&app_handle, false);
                if let Err(e) = app_handle
                    .notification()
                    .builder()
                    .title(format!("{} stopped", backend_label))
                    .body(format!(
                        "{} exited unexpectedly ({}). \
                         Open the tray menu and choose \"View Logs...\" for details.",
                        backend_label, status
                    ))
                    .show()
                {
                    warn!("Failed to show daemon-crash notification: {}", e);
                }
                return;
            }
        });

        info!("{} started", backend_name);
        Ok(())
    }

    /// Stop the ESPHome dashboard
    pub async fn stop(&self) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            info!("Daemon not running");
            return Ok(());
        }

        let backend_name = self.backend_name();
        info!("Stopping {}", backend_name);
        self.running.store(false, Ordering::SeqCst);

        let mut process = self.process.lock().await;
        if let Some(mut child) = process.take() {
            // Try graceful shutdown first - kill the process group on Unix
            #[cfg(unix)]
            {
                use nix::sys::signal::{killpg, Signal};
                use nix::unistd::Pid;

                if let Some(pid) = child.id() {
                    // Send SIGTERM to the process group to kill all children
                    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }

            // On Windows the graceful signal is CTRL_BREAK_EVENT to the
            // child's process group (Python surfaces it as SIGBREAK). Send
            // it up front, then wait the same patient window as Unix. Until
            // the backend installs a SIGBREAK handler the default action
            // terminates the child, so the wait returns promptly; once it
            // drains gracefully, the window gives it time. TerminateProcess
            // is the hard fallback on timeout (the only guarantee Windows
            // offers for a child that ignored the break).
            #[cfg(windows)]
            {
                if let Some(pid) = child.id() {
                    let _ = crate::platform::send_ctrl_break(pid);
                }
            }

            // Wait up to 30 s for the child to honor the signal and drain
            // in-flight work (firmware queue, partial writes, lock release);
            // we have measured up to 30 s in the wild.
            let timeout = tokio::time::timeout(tokio::time::Duration::from_secs(30), child.wait());

            match timeout.await {
                Ok(Ok(status)) => info!("{} exited with status: {}", backend_name, status),
                Ok(Err(e)) => warn!("Error waiting for process: {}", e),
                Err(_) => {
                    // On Unix we do NOT escalate to SIGKILL — force-killing
                    // corrupts dashboard state; we log and let the child
                    // finish on its own (it may briefly outlive us as an
                    // orphan). On Windows there is no gentler hard kill than
                    // TerminateProcess, so we use it as the last resort.
                    #[cfg(unix)]
                    warn!(
                        "Timeout waiting for {} to honor SIGTERM after 30 s; \
                         proceeding with exit without force-killing.",
                        backend_name
                    );
                    #[cfg(windows)]
                    {
                        warn!(
                            "Timeout waiting for {} to honor CTRL_BREAK after 30 s; \
                             force-killing.",
                            backend_name
                        );
                        let _ = child.kill().await;
                    }
                }
            }
        }

        self.dashboard_pid.store(0, Ordering::SeqCst);
        info!("{} stopped", backend_name);
        Ok(())
    }

    /// Synchronously terminate the dashboard child process. On Unix this
    /// sends SIGTERM to its process group; on Windows it delivers a
    /// graceful `CTRL_BREAK_EVENT` to its process group, with
    /// `TerminateProcess` as the fallback if the break can't be delivered.
    ///
    /// Safe to call from any context (including a tauri `RunEvent::Exit`
    /// callback where the tokio runtime is already winding down) — no
    /// async involvement. No-op if the daemon is not running or if a
    /// previous call already fired the kill.
    ///
    /// Idempotent via an atomic swap on `dashboard_pid` — repeated
    /// calls after the first are cheap no-ops, so it's safe to call
    /// from both the `ExitRequested` and `Exit` branches of the tauri
    /// run loop.
    ///
    /// Does NOT touch the `running` flag, so a concurrent / subsequent
    /// `stop()` still runs its wait. The PID atomic is only used for the
    /// synchronous kill path; `stop()` reads `child.id()` directly off
    /// the stored `Child` handle.
    ///
    /// On Unix this is SIGTERM only — the dashboard is expected to honor
    /// it and clean up its own state. On Windows the graceful signal is
    /// `CTRL_BREAK_EVENT`; we never hard-kill when the break was
    /// delivered (so a backend that handles SIGBREAK can drain), only
    /// when delivery fails. `TerminateProcess` is then the fallback
    /// because it is the only hard guarantee Windows offers.
    ///
    /// PID-reuse safety, Unix: guarded via `getpgid()`. If the dashboard
    /// child exited independently (crash / external kill) and tokio
    /// reaped it before we got here, the kernel may have handed our
    /// recorded PID to an unrelated process. We spawned the dashboard
    /// with `process_group(0)`, so the child is its own pgleader (pgid
    /// == pid). An unrelated process inheriting the recycled PID is
    /// almost certainly not its own pgleader, so a `getpgid(pid) != pid`
    /// result short-circuits the signal and we don't disturb the
    /// stranger.
    ///
    /// PID-reuse safety, Windows: structural. While tokio's `Child`
    /// handle for the process is open, Windows will not recycle that PID
    /// (a PID is freed only once all handles to the process object
    /// close). The handle lives in `process` and is dropped only by
    /// `stop()` or the exit-watcher, both of which also zero this
    /// atomic. So whenever `dashboard_pid != 0` the handle still pins
    /// the PID; no `getpgid` equivalent is needed. A stale/exited PID
    /// just makes `send_ctrl_break` (or the `OpenProcess` fallback) fail
    /// harmlessly.
    pub fn terminate_blocking(&self) {
        let pid = self.dashboard_pid.swap(0, Ordering::SeqCst);
        if pid == 0 {
            return;
        }
        #[cfg(unix)]
        {
            use nix::sys::signal::{killpg, Signal};
            use nix::unistd::{getpgid, Pid};
            let pid_t = Pid::from_raw(pid);
            match getpgid(Some(pid_t)) {
                Ok(pgid) if pgid == pid_t => {
                    let _ = killpg(pid_t, Signal::SIGTERM);
                }
                _ => {
                    warn!(
                        "Recorded dashboard pid {} is no longer its own \
                         process group leader; skipping SIGTERM to avoid \
                         signaling a recycled-PID stranger.",
                        pid
                    );
                }
            }
        }
        #[cfg(windows)]
        {
            // Graceful first: deliver CTRL_BREAK to the child's process
            // group. PID is stored as i32 to share the atomic with the Unix
            // path; the process-group id equals the child PID because we
            // spawned it with CREATE_NEW_PROCESS_GROUP.
            if !crate::platform::send_ctrl_break(pid as u32) {
                // The break could not be delivered (child gone, or no
                // reachable console). Fall back to TerminateProcess so the
                // child can never orphan.
                use ::windows::Win32::Foundation::CloseHandle;
                use ::windows::Win32::System::Threading::{
                    OpenProcess, TerminateProcess, PROCESS_TERMINATE,
                };
                // SAFETY: FFI into Win32. We pass a valid PID and immediately
                // close any handle we open; the handle never escapes this
                // block.
                unsafe {
                    match OpenProcess(PROCESS_TERMINATE, false, pid as u32) {
                        Ok(handle) => {
                            if let Err(e) = TerminateProcess(handle, 1) {
                                warn!("TerminateProcess on dashboard pid {} failed: {}", pid, e);
                            }
                            if let Err(e) = CloseHandle(handle) {
                                warn!("CloseHandle on dashboard pid {} failed: {}", pid, e);
                            }
                        }
                        Err(e) => warn!(
                            "OpenProcess on dashboard pid {} failed (already exited?): {}",
                            pid, e
                        ),
                    }
                }
            }
        }
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

/// Perform a health check on the dashboard
async fn health_check(port: u16) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let url = format!("http://localhost:{}/", port);
    match client.get(&url).send().await {
        Ok(response) => Ok(response.status().is_success()),
        Err(_) => Ok(false),
    }
}
