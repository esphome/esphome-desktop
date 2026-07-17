//! ESPHome daemon process management
//!
//! Handles starting, stopping, and monitoring the ESPHome dashboard process.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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

    /// The start sequence proper; see [`Self::start`] for the tray wrapper.
    async fn start_inner(&self) -> Result<()> {
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

        // Open log file for stdout and stderr combined.
        //
        // `File::create` truncates, so without rotating first every start wipes
        // the previous run's logs — leaving nothing to inspect after a failed
        // restart (issue #203). Rotate the prior `dashboard.log` to a numbered
        // backup first; best-effort, since losing old logs must never block the
        // backend from starting.
        let log_path = self.logs_dir.join(DASHBOARD_LOG_NAME);
        if let Err(e) = crate::util::rotate_log(&log_path, LOG_HISTORY) {
            warn!("Failed to rotate {:?}: {}", log_path, e);
        }
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

        // Keep the managed interpreter on its own tree: a stale package in the
        // user site directory otherwise shadows our pinned one and the backend
        // dies at import before it can serve anything (#318).
        platform::isolate_python_tokio_command(&mut cmd);

        // Set environment variables
        cmd.env("ESPHOME_DASHBOARD", "1");
        // Surface the desktop app version to the backend so it can be shown
        // in the frontend (e.g. an "About" page).
        cmd.env(
            "ESPHOME_DESKTOP_VERSION",
            self.app_handle.package_info().version.to_string(),
        );
        // Tell the backend where the esphome-desktop CLI lives so the dashboard
        // can check for and trigger updates through the stable `api` interface
        // (esphome-desktop api check-update / api update). Set beside the other
        // backend env vars and re-applied on every respawn like them; the
        // backend's own child processes inherit it too. See control::client.
        if let Some(bin) = crate::control::cli_invocation_path() {
            cmd.env("ESPHOME_DESKTOP_BIN", bin);
        }

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

        // Tie the backend's lifetime to ours so it can never be orphaned by an
        // exit path that doesn't run our code (uninstaller force-kill, crash,
        // Task Manager). See `platform::assign_to_kill_on_close_job` for why
        // this is needed on Windows specifically and why the job holds only the
        // child. Best-effort: if the job is unavailable we still have the
        // graceful CTRL_BREAK path, so log and carry on rather than failing the
        // start.
        //
        // There is a small window between CreateProcess and the assignment in
        // which a grandchild could escape the job. Closing it properly needs
        // CREATE_SUSPENDED plus a manual ResumeThread, which tokio doesn't
        // expose; Python has not spawned anything that early, so accept it.
        // `raw_handle()` is None only if the child has already been reaped, in
        // which case there is nothing to assign. Fold that in with assignment
        // failure: both mean the same thing to a reader of the log, and the
        // helper has already logged the underlying Win32 error, so this states
        // the consequence once rather than repeating the cause.
        // PROBE ONLY — DO NOT MERGE. Disabled to prove the e2e actually
        // detects the bug rather than passing either way.
        #[cfg(all(windows, not(feature = "probe-no-job")))]
        if !child
            .raw_handle()
            .is_some_and(platform::assign_to_kill_on_close_job)
        {
            warn!(
                "{BACKEND_NAME} is not covered by the kill-on-close job; it may outlive \
                 the desktop if this process is killed without running its shutdown path"
            );
        }

        if let Some(pid) = child.id() {
            self.dashboard_pid.store(pid as PidInt, Ordering::SeqCst);
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
                    .title(crate::i18n::t_with(
                        "daemon.stopped_title",
                        &[("backend", &backend_label)],
                    ))
                    .body(crate::i18n::t_with(
                        "daemon.stopped_body",
                        &[
                            ("backend", backend_label.as_str()),
                            ("status", &status.to_string()),
                        ],
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

    /// Stop the ESPHome dashboard.
    ///
    /// Emits the tray status itself: "Stopped" optimistically up front (the
    /// graceful drain below can take up to 30s, during which the tray should
    /// not claim the backend is running), restored to the actual state if the
    /// stop fails — after a failed stop the backend may well still be
    /// running, so the optimistic label must not stand.
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

    /// The stop sequence proper; see [`Self::stop`] for the tray wrapper.
    async fn stop_inner(&self) -> Result<()> {
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

        // Do NOT clear `running` yet. The health-check and exit-watcher tasks
        // spawned in start() retire themselves when `running` goes false, and
        // the Unix drain below may time out with the backend still alive — in
        // which case we keep the process and must keep its watchers. Clearing
        // `running` only after a *confirmed* stop (see the bottom of this fn)
        // means the timeout path leaves both the flag and the watchers intact,
        // so a later backend exit still clears state and monitoring survives a
        // failed stop attempt. The tray already shows "Stopped" optimistically
        // via stop()'s wrapper, so the label isn't tied to this flag.
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
                // A wait() error on Unix almost always means the child was
                // already reaped (ECHILD) — i.e. it exited before we waited —
                // so we treat this as a confirmed stop and fall through to
                // clear state below, unlike the timeout arm which bail!s.
                Ok(Err(e)) => warn!("Error waiting for process: {}", e),
                Err(_) => {
                    // On Unix we do NOT escalate to SIGKILL — force-killing
                    // corrupts dashboard state. The backend is still alive, so
                    // we cannot honestly report a successful stop: put the child
                    // handle back and return Err. `running` was never cleared
                    // (we defer that until a confirmed stop, above), so the
                    // watcher tasks from start() are still live and keep
                    // monitoring the restored child. Callers such as the
                    // channel/backend switch flows depend on this to abort
                    // *before* pip-installing over a live process; `stop()`'s
                    // tray wrapper depends on `running` staying true to keep the
                    // running label standing.
                    #[cfg(unix)]
                    {
                        warn!(
                            "Timeout waiting for {} to honor SIGTERM after 30 s; \
                             not force-killing (would corrupt dashboard state) — \
                             reporting stop failure so callers can abort.",
                            backend_name
                        );
                        *process = Some(child);
                        anyhow::bail!("timed out waiting for {} to stop", backend_name);
                    }
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

        // Confirmed stop (child exited, wait errored as already-reaped, or the
        // Windows force-kill fired). Only now clear `running`, which retires the
        // watcher tasks; the Unix drain-timeout path bailed out above and left
        // this flag true so its watchers live on.
        self.running.store(false, Ordering::SeqCst);
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
