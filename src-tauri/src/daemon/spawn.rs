//! Bringing the backend up: the spawn recipe and the tasks that watch it.
//!
//! `start_inner` owns everything between "no child" and "a running child with
//! its watchers installed": log rotation, the command's stdio / process-group /
//! environment setup, and the two supervision tasks (health check and exit
//! watcher) that retire themselves once a newer start supersedes them. The
//! tray-emitting wrapper stays in the parent, see `DaemonManager::start`.

use anyhow::{Context, Result};
use std::fs::File;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use tauri_plugin_notification::NotificationExt;
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use super::{health_check, DaemonManager, PidInt, BACKEND_NAME, DASHBOARD_LOG_NAME, LOG_HISTORY};
use crate::platform;

impl DaemonManager {
    /// The start sequence proper; see [`Self::start`] for the tray wrapper.
    pub(super) async fn start_inner(&self) -> Result<()> {
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
        // `raw_handle()` is None only if the child was already reaped, which
        // needs no assignment either — hence `is_some_and`. The helper logs the
        // Win32 cause; this logs the consequence.
        #[cfg(windows)]
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
}
