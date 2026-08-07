//! Taking the backend down, and the contract both paths share.
//!
//! Both paths lead with a graceful signal — SIGTERM to the child's process
//! group on Unix, `CTRL_BREAK_EVENT` on Windows — because killing the
//! dashboard outright corrupts its state. `stop_inner` is the async path
//! (signal, then drain for up to 30 s); `terminate_blocking` is the
//! synchronous one, for exit callbacks that cannot await.
//!
//! What happens when the graceful signal doesn't take is platform-split. On
//! Unix we never escalate: `stop_inner` restores the child and reports the
//! failure, and `terminate_blocking` has no fallback. On Windows both escalate
//! — `stop_inner` calls `kill()` after the drain window, `terminate_blocking`
//! falls back to `TerminateProcess` when the break can't be delivered —
//! because that hard kill is the only shutdown guarantee Windows offers.

use anyhow::Result;
use std::sync::atomic::Ordering;
use tracing::{info, warn};

use super::{DaemonManager, BACKEND_NAME};

impl DaemonManager {
    /// The stop sequence proper; see [`Self::stop`] for the tray wrapper.
    pub(super) async fn stop_inner(&self) -> Result<()> {
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
}
