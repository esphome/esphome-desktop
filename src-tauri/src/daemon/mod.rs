//! ESPHome daemon process management
//!
//! Handles starting, stopping, and monitoring the ESPHome dashboard process.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::platform;
use crate::settings::Settings;

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

/// How long `start()` waits for a previous backend process to exit before
/// spawning anyway. Covers the documented worst-case SIGTERM drain of the
/// Python backend (20-60s); a graceful relaunch finds the old process already
/// gone and never waits.
const PREVIOUS_BACKEND_EXIT_TIMEOUT: Duration = Duration::from_secs(60);

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
    /// File holding the spawned backend's PID. A relaunched process (e.g. after
    /// an app update) no longer holds the old `Child` handle, so it reads this
    /// to wait for the previous backend to exit before binding the mDNS socket.
    pid_file: PathBuf,
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
        let config_dir = settings.config_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("esphome"))
                .unwrap_or_else(|| PathBuf::from("esphome"))
        });
        std::fs::create_dir_all(&config_dir).context("Failed to create config directory")?;

        // Create logs directory in app data
        let logs_dir = data_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).context("Failed to create logs directory")?;
        let pid_file = logs_dir.join("device-builder.pid");

        Ok(Self {
            process: Arc::new(Mutex::new(None)),
            python_path,
            python_bin_dir,
            config_dir,
            logs_dir,
            pid_file,
            port: settings.port,
            running: Arc::new(AtomicBool::new(false)),
            dashboard_pid: Arc::new(AtomicPid::new(0)),
            app_handle: app_handle.clone(),
        })
    }

    /// Start the ESPHome device builder
    pub async fn start(&self) -> Result<()> {
        // Wait for a previous backend to exit before spawning. On an app-update
        // relaunch our stop() can return while the old backend is still draining
        // its SIGTERM (its handler outlives the 30s stop wait), and that orphan
        // keeps holding UDP 5353; a new backend would then co-bind 5353 via
        // SO_REUSEPORT and split the mDNS replies, so devices show offline until
        // it dies. The old `Child` handle is gone across the relaunch exec, so we
        // wait on its pid from the pid file. Done BEFORE taking the process lock
        // so a Stop / Restart can still be serviced while we wait.
        Self::wait_for_previous_backend_exit(&self.pid_file, PREVIOUS_BACKEND_EXIT_TIMEOUT).await;

        // Hold the process lock for the entire start sequence (check ->
        // spawn -> store) so two concurrent start() calls can't both pass
        // the running check and each spawn a child. Without this, the
        // second `*process = Some(child)` would drop the first Child; on
        // Unix we deliberately don't set `kill_on_drop`, so that dropped
        // child is never signaled and orphans a stray dashboard process.
        // stop() also takes this lock, so start()/stop() are serialized too.
        //
        // Consequence: stop() holds this lock across its up-to-30s child drain,
        // so a stop-then-start sequence (Restart, or rapid Stop->Start) makes
        // start() await the lock until stop() finishes — up to 30s. This is the
        // intended serialization (prevents the new dashboard racing the old one
        // for the port). start() is async, so it yields rather than blocking a
        // thread, keeping the tray/UI responsive.
        let mut process = self.process.lock().await;

        if self.running.load(Ordering::SeqCst) {
            info!("Daemon already running");
            return Ok(());
        }

        let backend_name = BACKEND_NAME;
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
        let log_file_clone = log_file
            .try_clone()
            .context("Failed to clone log file handle")?;

        info!("{} logs: {:?}", backend_name, log_path);

        let config_arg = self.config_dir.to_str().unwrap_or(".");
        let port_arg = self.port.to_string();

        // Build the command
        let mut cmd = Command::new(&self.python_path);
        cmd.args([
            "-m",
            "esphome_device_builder",
            config_arg,
            "--host",
            "127.0.0.1",
            "--port",
            &port_arg,
        ]);
        cmd
            // Set working directory to config dir (required for PlatformIO)
            .current_dir(&self.config_dir)
            // Redirect stdout/stderr to single log file
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_file_clone));

        // Give the daemon a null stdin instead of inheriting ours. The
        // dashboard/device-builder never reads stdin, so there is no reason to
        // hold a handle to it on any platform.
        //
        // On Windows this is also load-bearing for restart: the shutdown path
        // calls `platform::send_ctrl_break`, whose `AttachConsole`/`FreeConsole`
        // dance mutates this (GUI, console-less) process's standard handles.
        // `STD_INPUT_HANDLE` starts out NULL but is left dangling once we attach
        // to and then free the child's console. A subsequent restart respawn
        // would inherit that invalid handle, and because stdout/stderr are
        // redirected (so `STARTF_USESTDHANDLES` is set and *all three* handles
        // must be valid) `CreateProcess` fails with ERROR_INVALID_HANDLE (os
        // error 6) — leaving the daemon dead after every restart. Pinning stdin
        // to a known-good handle makes the spawn independent of our
        // console-handle state.
        cmd.stdin(Stdio::null());

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
        // in the frontend (e.g. an "About" page).
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
            self.dashboard_pid.store(pid as PidInt, Ordering::SeqCst);
            // Record the pid so a future relaunch (which loses this Child handle
            // across the exec) can wait for this backend to exit before spawning.
            if let Err(e) = std::fs::write(&self.pid_file, pid.to_string()) {
                warn!(
                    "Failed to write backend pid file {:?}: {}",
                    self.pid_file, e
                );
            }
        }

        *process = Some(child);
        self.running.store(true, Ordering::SeqCst);
        // Release the lock before spawning the watcher tasks below; they
        // re-acquire it on their own polling cadence.
        drop(process);

        // Start health check task.
        //
        // Like the exit-watcher below, this captures the child's PID at spawn
        // time and exits once `dashboard_pid` no longer matches. A
        // stop()/start() pair faster than the 30s poll interval flips `running`
        // false then back to true, so the `running` check alone wouldn't retire
        // a superseded task: the old health-check loop would wake to
        // running=true (set by the new start()) and keep probing forever,
        // leaking one task per restart. The PID guard retires it as soon as a
        // newer start() installs its own watcher.
        let running = self.running.clone();
        let port = self.port;
        let health_dashboard_pid = self.dashboard_pid.clone();
        let health_watcher_pid = self.dashboard_pid.load(Ordering::SeqCst);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                if health_dashboard_pid.load(Ordering::SeqCst) != health_watcher_pid {
                    // Superseded by a newer start(); its task probes now.
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
        let pid_file = self.pid_file.clone();
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
                // The child is reaped; clear its pid file so a later start()
                // doesn't wait on a dead (or recycled) pid.
                DaemonManager::remove_pid_file(&pid_file);

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

    /// Wait for a previous backend process to exit before spawning a new one.
    ///
    /// Polls the pid recorded at the last spawn until that process is gone, or
    /// `max_wait` elapses. We wait on the pid (not a port) because the backend
    /// releases its TCP port before its mDNS socket during shutdown, so a free
    /// port does not imply the old mDNS responder is gone. Best effort: a stuck
    /// backend past the deadline is logged and we spawn anyway (better a briefly
    /// degraded mDNS than no dashboard). No pid file, or an already-gone pid,
    /// returns at once.
    async fn wait_for_previous_backend_exit(pid_file: &Path, max_wait: Duration) {
        let Some(pid) = Self::read_pid_file(pid_file) else {
            return;
        };
        let deadline = Instant::now() + max_wait;
        let mut warned = false;
        loop {
            if !Self::previous_backend_alive(pid) {
                return;
            }
            if !warned {
                warn!(
                    "Previous backend (pid {}) still draining; waiting up to {:?} \
                     for it to exit before spawning",
                    pid, max_wait
                );
                warned = true;
            }
            if Instant::now() >= deadline {
                warn!(
                    "Previous backend (pid {}) still alive after {:?}; spawning anyway",
                    pid, max_wait
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Read the recorded backend pid, or `None` if absent / unparseable / zero.
    fn read_pid_file(pid_file: &Path) -> Option<PidInt> {
        let pid: PidInt = std::fs::read_to_string(pid_file)
            .ok()?
            .trim()
            .parse()
            .ok()?;
        (pid != 0).then_some(pid)
    }

    /// Remove the pid file; a missing file is not an error.
    fn remove_pid_file(pid_file: &Path) {
        if let Err(e) = std::fs::remove_file(pid_file) {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Failed to remove backend pid file {:?}: {}", pid_file, e);
            }
        }
    }

    /// Whether the recorded pid is still our live backend.
    ///
    /// Unix: the backend is spawned `process_group(0)`, so a live one is its own
    /// process-group leader; a recycled pid almost certainly is not, so
    /// `getpgid(pid) != pid` reads as "gone" and we never wait on a stranger
    /// (the same guard `terminate_blocking` uses).
    #[cfg(unix)]
    fn previous_backend_alive(pid: PidInt) -> bool {
        use nix::unistd::{getpgid, Pid};
        let pid_t = Pid::from_raw(pid);
        matches!(getpgid(Some(pid_t)), Ok(pgid) if pgid == pid_t)
    }

    /// Whether the recorded pid is still alive.
    ///
    /// Windows: a still-running process reports exit code `STILL_ACTIVE` (259);
    /// a pid that can't be opened is treated as gone.
    #[cfg(windows)]
    fn previous_backend_alive(pid: PidInt) -> bool {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: FFI into Win32; the handle never escapes and is closed before
        // return.
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return false;
            };
            let mut code: u32 = 0;
            let alive = GetExitCodeProcess(handle, &mut code).is_ok() && code == 259;
            let _ = CloseHandle(handle);
            alive
        }
    }

    /// Stop the ESPHome dashboard
    pub async fn stop(&self) -> Result<()> {
        // Acquire the process lock *before* reading/mutating `running` so the
        // check-and-act is fully atomic against start(), which also reads
        // `running` under this lock. The check is done post-lock (no lockless
        // fast-path) so a Stop click in the narrow window where start() holds
        // the lock but hasn't yet stored running=true can't no-op and leave
        // the just-started dashboard running. Without holding the lock across
        // the running check, a stop->start overlap could also let start() win
        // the lock, see running=false, and `*process = Some(child)` drop the
        // old Child without signaling it (no kill_on_drop on Unix), orphaning
        // the old dashboard.
        let mut process = self.process.lock().await;
        if !self.running.load(Ordering::SeqCst) {
            info!("Daemon not running");
            return Ok(());
        }

        let backend_name = BACKEND_NAME;
        info!("Stopping {}", backend_name);

        self.running.store(false, Ordering::SeqCst);
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
                Ok(Ok(status)) => {
                    info!("{} exited with status: {}", backend_name, status);
                    // Confirmed gone; clear the pid file so the next start()
                    // doesn't wait on it. On timeout we deliberately leave it so
                    // a relaunch waits for the still-draining orphan.
                    Self::remove_pid_file(&self.pid_file);
                }
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
            // group. The process-group id equals the child PID because we
            // spawned it with CREATE_NEW_PROCESS_GROUP.
            if !crate::platform::send_ctrl_break(pid) {
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
                    match OpenProcess(PROCESS_TERMINATE, false, pid) {
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

/// Build the health-check URL for the dashboard.
///
/// The backend is spawned with `--address 127.0.0.1` / `--host 127.0.0.1`
/// (see `start()`), so it only listens on the IPv4 loopback. Probing the
/// literal `127.0.0.1` rather than the `localhost` hostname avoids a
/// resolver detour: on IPv6-first hosts `localhost` resolves to `::1`
/// first, where nothing is listening, producing spurious health-check
/// failures (and a 5s connect stall per cycle before the IPv4 fallback).
fn health_check_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/", port)
}

/// Perform a health check on the dashboard
async fn health_check(port: u16) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let url = health_check_url(port);
    match client.get(&url).send().await {
        Ok(response) => Ok(response.status().is_success()),
        Err(_) => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::{health_check_url, DaemonManager, PidInt};
    use std::path::PathBuf;

    #[test]
    fn health_check_url_targets_ipv4_loopback() {
        // Must match the address the backend binds (`127.0.0.1`), not the
        // `localhost` hostname, so the probe doesn't get steered to `::1`
        // on IPv6-first hosts where the daemon isn't listening.
        assert_eq!(health_check_url(6052), "http://127.0.0.1:6052/");
        assert!(!health_check_url(6052).contains("localhost"));
    }

    fn unique_pid_file(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("esphome_pidfile_{}_{tag}", std::process::id()))
    }

    #[test]
    fn read_pid_file_absent_is_none() {
        let path = unique_pid_file("absent");
        let _ = std::fs::remove_file(&path);
        assert_eq!(DaemonManager::read_pid_file(&path), None);
    }

    #[test]
    fn read_pid_file_parses_trimmed_pid() {
        let path = unique_pid_file("valid");
        std::fs::write(&path, "12345\n").unwrap();
        assert_eq!(DaemonManager::read_pid_file(&path), Some(12345 as PidInt));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_file_rejects_zero_and_garbage() {
        let path = unique_pid_file("bad");
        std::fs::write(&path, "0").unwrap();
        assert_eq!(DaemonManager::read_pid_file(&path), None);
        std::fs::write(&path, "not-a-pid").unwrap();
        assert_eq!(DaemonManager::read_pid_file(&path), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn previous_backend_alive_false_for_dead_pid() {
        // No process owns PidInt::MAX, so it must read as gone (not alive)
        // and never make start() wait on a stranger.
        assert!(!DaemonManager::previous_backend_alive(PidInt::MAX));
    }

    #[test]
    fn remove_pid_file_is_idempotent() {
        let path = unique_pid_file("remove");
        std::fs::write(&path, "999").unwrap();
        DaemonManager::remove_pid_file(&path);
        assert!(!path.exists());
        // Second removal of an already-missing file must not panic or warn-fail.
        DaemonManager::remove_pid_file(&path);
    }
}
