//! Shared control operations.
//!
//! The multi-step stop→install→start sequences behind the tray's Switch
//! Channel / Switch Backend / Restart Dashboard items and their CLI
//! equivalents. The tray arms wrap these with confirmation dialogs; the
//! control server wraps them with streamed progress replies. Keeping the
//! sequences here means both surfaces stay in lockstep, including the tray
//! label updates (which are safe no-ops when the app runs without a tray —
//! exactly the situation the CLI exists for).
//!
//! This module holds what the sequences share — the [`UpdateGuard`] that makes
//! them mutually exclusive, the [`Progress`] sink they report through, and the
//! [`stop_install_start`] skeleton two of them are built on. The sequences
//! themselves live in the submodules and are re-exported here, so call sites
//! keep addressing them as `ops::…`:
//!
//! * [`switch`] — Switch Channel, Switch Backend, Restart Dashboard;
//! * [`startup`] — the launch-at-login toggle;
//! * [`full_update`] — the CLI's non-interactive update flow.

mod full_update;
mod startup;
mod switch;

pub(crate) use full_update::{run_full_update, UpdateReport};
pub(crate) use startup::{set_launch_at_startup, startup_enabled};
pub(crate) use switch::{restart_daemon, switch_backend, switch_release_channel, SwitchOutcome};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tracing::{error, warn};

use crate::AppState;

/// Progress sink for long-running operations: `(step, detail)`. The tray
/// passes a no-op (its feedback is dialogs); the control server forwards each
/// call to the client as a [`super::protocol::Reply::Progress`] line.
pub(crate) type Progress<'a> = &'a (dyn Fn(&str, &str) + Send + Sync);

/// RAII guard ensuring only one update/switch sequence runs at a time.
///
/// The "Check for Updates", "Switch Channel", and "Switch Backend" tray arms —
/// and their CLI counterparts — each perform a multi-step
/// stop→install/update→start sequence. `DaemonManager::start()`/`stop()` are
/// individually mutex-serialized, but those *sequences* are not mutually
/// exclusive, so concurrent triggers (a fast double-click, or a CLI call while
/// a tray dialog is open) could interleave the steps at `await` points and
/// stack dialogs.
///
/// Acquiring this guard at the top of each sequence makes them mutually
/// exclusive: a second trigger while one is in flight is rejected. The flag
/// is released on drop, so every early `return`/`?` path frees it
/// automatically.
pub(crate) struct UpdateGuard(Arc<AtomicBool>);

impl UpdateGuard {
    /// Try to begin an update/switch sequence. Returns `None` if one is already
    /// in flight (i.e. the flag was already `true`).
    pub(crate) fn try_acquire(flag: Arc<AtomicBool>) -> Option<Self> {
        if flag
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            Some(Self(flag))
        } else {
            None
        }
    }

    /// Acquire the guard, waiting for any in-flight sequence to finish. Used
    /// by the startup daemon-start task, which must run to completion rather
    /// than bail like the user-triggered sequences do.
    pub(crate) async fn acquire_wait(flag: Arc<AtomicBool>) -> Self {
        loop {
            match Self::try_acquire(flag.clone()) {
                Some(guard) => return guard,
                None => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// How long the readiness probe waits for the dashboard after a restart or
/// switch. Also quoted in the client-facing not-ready messages.
pub(crate) const READY_TIMEOUT_SECS: u64 = 60;

/// Shared suffix for the client-facing "it restarted but never answered"
/// messages, so the quoted timeout can't drift from [`READY_TIMEOUT_SECS`].
pub(crate) fn not_ready_note() -> String {
    format!("did not become ready within {READY_TIMEOUT_SECS}s; check the logs")
}

/// Apply `mutate` to the settings under the write lock and persist them,
/// downgrading a save failure to a warning (the in-memory value still
/// stands). `mutate` returns whether anything changed; an unchanged result
/// skips the save.
pub(super) async fn set_and_save<F>(app: &AppHandle, state: &Arc<AppState>, mutate: F)
where
    F: FnOnce(&mut crate::settings::Settings) -> bool,
{
    let mut settings = state.settings.write().await;
    if mutate(&mut settings) {
        if let Err(e) = settings.save(app) {
            warn!("Failed to save settings: {}", e);
        }
    }
}

/// How a [`stop_install_start`] sequence ended. The tray maps these onto
/// dialogs, [`run_package_phase`] onto the CLI report's lines.
///
/// Deliberately not folded into a `Result<(), String>`: the surfaces disagree
/// on what each failure means. A failed start is a warning to the tray (the
/// new version *is* installed) but an error to the CLI, and only the CLI
/// mentions a failed post-install restart at all.
pub(crate) enum InstallOutcome {
    /// Installed, and the dashboard came back.
    Installed,
    /// The dashboard could not be stopped, so nothing was installed.
    StopFailed(String),
    /// The install failed. The dashboard was restarted anyway; `restart_error`
    /// carries the reason when even that did not work.
    InstallFailed {
        error: String,
        restart_error: Option<String>,
    },
    /// The install succeeded but the dashboard did not come back.
    StartFailed(String),
}

/// Stop the dashboard, run `install`, then start the dashboard again — the
/// sequence behind both the tray's Check for Updates arm and the CLI's update
/// command, so the two cannot drift. `what` names the operation in the log
/// lines this emits on every failure arm, leaving callers to render outcomes
/// rather than re-log them.
///
/// The start is attempted even after a failed install, so a user is never left
/// without a dashboard. `refresh` (the tray version display) runs between a
/// successful install and the start, matching the switch flows: the new
/// version is on disk from that moment, so the tray must show it even if the
/// start then fails.
pub(crate) async fn stop_install_start<F, Fut, R, RFut>(
    state: &Arc<AppState>,
    what: &str,
    install: F,
    refresh: R,
) -> InstallOutcome
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = ()>,
{
    if let Err(e) = state.daemon.stop().await {
        error!("Failed to stop the dashboard for the {}: {}", what, e);
        return InstallOutcome::StopFailed(e.to_string());
    }
    let install_result = install().await;
    if install_result.is_ok() {
        refresh().await;
    }
    let start_result = state.daemon.start().await;
    match (install_result, start_result) {
        (Ok(()), Ok(())) => InstallOutcome::Installed,
        (Ok(()), Err(e)) => {
            error!("Failed to restart the dashboard after the {}: {}", what, e);
            InstallOutcome::StartFailed(e.to_string())
        }
        (Err(e), start) => {
            error!("The {} failed: {}", what, e);
            let restart_error = start.err().map(|start_err| {
                error!(
                    "Failed to restart the dashboard after the failed {}: {}",
                    what, start_err
                );
                start_err.to_string()
            });
            InstallOutcome::InstallFailed {
                error: e.to_string(),
                restart_error,
            }
        }
    }
}

/// Best-effort dashboard restart after a failed install, so the user isn't
/// left without a backend. Returns whether the restart succeeded.
pub(super) async fn restart_after_failure(state: &Arc<AppState>, context: &str) -> bool {
    match state.daemon.start().await {
        Ok(()) => true,
        Err(e) => {
            error!("Failed to restart backend after {}: {}", context, e);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guard_acquires_when_flag_clear() {
        let flag = Arc::new(AtomicBool::new(false));
        let g = UpdateGuard::try_acquire(flag.clone());
        assert!(g.is_some(), "should acquire when flag is clear");
        assert!(flag.load(Ordering::Acquire), "flag set while guard held");
    }

    #[test]
    fn guard_blocks_second_acquire_while_held() {
        let flag = Arc::new(AtomicBool::new(false));
        let _first = UpdateGuard::try_acquire(flag.clone()).expect("first acquires");
        let second = UpdateGuard::try_acquire(flag.clone());
        assert!(
            second.is_none(),
            "second acquire blocked while first is held"
        );
    }

    #[test]
    fn guard_releases_flag_on_drop() {
        let flag = Arc::new(AtomicBool::new(false));
        {
            let _g = UpdateGuard::try_acquire(flag.clone()).expect("acquires");
            assert!(flag.load(Ordering::Acquire), "held");
        }
        assert!(
            !flag.load(Ordering::Acquire),
            "flag cleared after guard dropped"
        );
        assert!(
            UpdateGuard::try_acquire(flag.clone()).is_some(),
            "reacquirable after release"
        );
    }
}
