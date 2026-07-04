//! Shared control operations.
//!
//! The multi-step stop→install→start sequences behind the tray's Switch
//! Channel / Switch Backend / Restart Dashboard items and their CLI
//! equivalents. The tray arms wrap these with confirmation dialogs; the
//! control server wraps them with streamed progress replies. Keeping the
//! sequences here means both surfaces stay in lockstep, including the tray
//! label updates (which are safe no-ops when the app runs without a tray —
//! exactly the situation the CLI exists for).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_updater::UpdaterExt;
use tracing::{error, info, warn};

use crate::settings::ReleaseChannel;
use crate::{tray, AppState};

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

/// Show the daemon's actual state in the tray. The switch/restart sequences
/// optimistically set "stopped" before stopping; after a failed stop the
/// backend may well still be running, so the optimistic label must not stand.
fn show_actual_status(app: &AppHandle, state: &AppState) {
    tray::update_status(app, state.daemon.is_running());
}

/// How a channel/backend switch ended. The tray maps these onto dialogs, the
/// control server onto terminal replies.
pub(crate) enum SwitchOutcome {
    /// The requested value was already active; nothing was done.
    Unchanged,
    /// Switched and restarted. `ready` is whether the dashboard answered the
    /// readiness probe (only probed by the backend switch; the channel switch
    /// reports `true` without probing, matching the previous tray behavior).
    Success { ready: bool },
    /// The dashboard could not be stopped; nothing was installed.
    StopFailed(String),
    /// The install failed; `restarted` is whether the previous version's
    /// dashboard came back up.
    InstallFailed { error: String, restarted: bool },
    /// The install succeeded but the dashboard failed to start afterwards.
    StartFailed(String),
}

/// Switch the ESPHome release channel: stop the dashboard, install the new
/// channel's version, persist the setting, and restart. Tray radio labels and
/// the status line are updated (and reverted on failure) along the way.
pub(crate) async fn switch_release_channel(
    app: &AppHandle,
    state: &Arc<AppState>,
    new_channel: ReleaseChannel,
    _guard: &UpdateGuard,
    progress: Progress<'_>,
) -> SwitchOutcome {
    let old_channel = state.settings.read().await.release_channel;
    if new_channel == old_channel {
        return SwitchOutcome::Unchanged;
    }

    // Show the new selection immediately; reverted on failure below.
    tray::update_channel_checks(new_channel);

    progress("stop", "stopping the dashboard");
    tray::update_status(app, false);
    if let Err(e) = state.daemon.stop().await {
        error!("Failed to stop backend for channel switch: {}", e);
        tray::update_channel_checks(old_channel);
        show_actual_status(app, state);
        return SwitchOutcome::StopFailed(e.to_string());
    }

    progress(
        "install",
        &format!("installing ESPHome from the {} channel", new_channel),
    );
    match state.update_checker.switch_channel(app, new_channel).await {
        Ok(()) => {
            info!("Switched to {} channel successfully", new_channel);

            {
                let mut settings = state.settings.write().await;
                settings.release_channel = new_channel;
                if let Err(e) = settings.save(app) {
                    warn!("Failed to save settings: {}", e);
                }
            }

            refresh_version_display_blocking(app).await;

            progress("start", "starting the dashboard");
            if let Err(e) = state.daemon.start().await {
                error!("Failed to restart backend after channel switch: {}", e);
                return SwitchOutcome::StartFailed(e.to_string());
            }
            tray::update_status(app, true);
            SwitchOutcome::Success { ready: true }
        }
        Err(e) => {
            error!("Channel switch failed: {}", e);
            tray::update_channel_checks(old_channel);
            let restarted = restart_after_failure(app, state, "failed channel switch").await;
            SwitchOutcome::InstallFailed {
                error: e.to_string(),
                restarted,
            }
        }
    }
}

/// Switch the device-builder backend channel: stop the dashboard, install the
/// package for the new channel, persist the setting, restart, and wait for the
/// dashboard to become reachable.
pub(crate) async fn switch_backend(
    app: &AppHandle,
    state: &Arc<AppState>,
    new_backend: crate::settings::Backend,
    _guard: &UpdateGuard,
    progress: Progress<'_>,
) -> SwitchOutcome {
    let old_backend = state.settings.read().await.backend;
    if new_backend == old_backend {
        return SwitchOutcome::Unchanged;
    }

    tray::update_backend_checks(new_backend);

    progress("stop", "stopping the backend");
    tray::update_status(app, false);
    if let Err(e) = state.daemon.stop().await {
        error!("Failed to stop daemon for backend switch: {}", e);
        tray::update_backend_checks(old_backend);
        show_actual_status(app, state);
        return SwitchOutcome::StopFailed(e.to_string());
    }

    // Install/upgrade the package for the selected channel first.
    progress(
        "install",
        &format!("installing esphome-device-builder ({new_backend})"),
    );
    if let Err(e) = state
        .update_checker
        .install_device_builder(app, new_backend)
        .await
    {
        error!("Failed to install esphome-device-builder: {}", e);
        tray::update_backend_checks(old_backend);
        let restarted = restart_after_failure(app, state, "failed backend switch").await;
        return SwitchOutcome::InstallFailed {
            error: e.to_string(),
            restarted,
        };
    }
    // Install succeeded — refresh the tray version display.
    tray::refresh_builder_version_display(app).await;

    // Persist the new backend channel.
    {
        let mut settings = state.settings.write().await;
        settings.backend = new_backend;
        if let Err(e) = settings.save(app) {
            warn!("Failed to save settings: {}", e);
        }
    }

    progress("start", "starting the backend");
    if let Err(e) = state.daemon.start().await {
        error!("Failed to start daemon after backend switch: {}", e);
        return SwitchOutcome::StartFailed(e.to_string());
    }
    tray::update_status(app, true);
    info!("Switched backend to {}", new_backend);

    progress("wait", "waiting for the backend to become ready");
    let port = state.daemon.port();
    let ready = crate::wait_for_dashboard_ready(port, READY_TIMEOUT_SECS).await;
    SwitchOutcome::Success { ready }
}

/// Restart the dashboard backend. With `wait_ready` the call also polls the
/// dashboard's readiness probe and returns whether it came up within 60s.
pub(crate) async fn restart_daemon(
    app: &AppHandle,
    state: &Arc<AppState>,
    wait_ready: bool,
    _guard: &UpdateGuard,
    progress: Progress<'_>,
) -> Result<bool, String> {
    progress("restart", "restarting the dashboard");
    tray::update_status(app, false);
    if let Err(e) = state.daemon.restart().await {
        show_actual_status(app, state);
        return Err(e.to_string());
    }
    tray::update_status(app, true);
    if !wait_ready {
        return Ok(true);
    }
    progress("wait", "waiting for the dashboard to become ready");
    Ok(crate::wait_for_dashboard_ready(state.daemon.port(), READY_TIMEOUT_SECS).await)
}

/// Serializes launch-at-login toggles: concurrent toggles (two fast tray
/// clicks, or tray + CLI) could otherwise run their OS enable/disable calls
/// in the opposite order of their settings writes, leaving the login item
/// contradicting the setting. A dedicated lock rather than the settings lock,
/// so a slow OS call doesn't block unrelated settings readers.
static STARTUP_TOGGLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Persist the autostart preference, reconcile the OS login item, and refresh
/// the tray radio labels. Returns the actual post-call OS state, which is what
/// callers should report — enable/disable can fail (permissions, policy,
/// platform limits), and reporting the requested state would mislead.
pub(crate) async fn set_launch_at_startup(
    app_handle: &AppHandle,
    state: &Arc<AppState>,
    enable: bool,
) -> bool {
    let _toggle = STARTUP_TOGGLE.lock().await;
    {
        let mut settings = state.settings.write().await;
        if settings.launch_at_startup != enable {
            settings.launch_at_startup = enable;
            if let Err(e) = settings.save(app_handle) {
                warn!("Failed to save settings: {}", e);
            }
        }
    }

    // Always (re)apply the OS call, even when the persisted value already
    // matches, so an already-selected choice retries a registration that
    // failed earlier (e.g. the startup reconcile) instead of no-opping.
    // The plugin's calls are blocking OS work (macOS can shell out to
    // System Events), so keep them off the async runtime.
    let app = app_handle.clone();
    let actual = tokio::task::spawn_blocking(move || {
        let manager = app.autolaunch();
        let result = if enable {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(e) = result {
            error!(
                "Failed to {} autostart: {}",
                if enable { "enable" } else { "disable" },
                e
            );
        }
        // Fall back to the requested value only if the state query itself
        // fails. The persisted setting keeps the user's intent, so the
        // launch-time reconcile retries.
        manager.is_enabled().unwrap_or(enable)
    })
    .await
    .unwrap_or(enable);
    tray::update_startup_checks(actual);
    actual
}

/// Actual OS login-item state, falling back to the persisted intent when the
/// query fails. Runs the blocking plugin call off the async runtime.
pub(crate) async fn startup_enabled(app_handle: &AppHandle, fallback: bool) -> bool {
    let app = app_handle.clone();
    tokio::task::spawn_blocking(move || app.autolaunch().is_enabled().unwrap_or(fallback))
        .await
        .unwrap_or(fallback)
}

/// Run a blocking version-detection function (they spawn Python subprocesses)
/// off the async runtime, flattening the join error into the detection error.
pub(crate) async fn detect<T, F>(app: &AppHandle, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&AppHandle) -> anyhow::Result<T> + Send + 'static,
{
    let app = app.clone();
    match tokio::task::spawn_blocking(move || f(&app)).await {
        Ok(result) => result.map_err(|e| e.to_string()),
        Err(join) => Err(join.to_string()),
    }
}

/// Result of [`run_full_update`].
pub(crate) struct UpdateReport {
    /// A desktop self-update was installed; the caller must relaunch the app
    /// (after flushing its reply — the relaunch kills the connection).
    pub app_update_installed: bool,
    /// One human-readable line per component.
    pub lines: Vec<String>,
    /// Whether any component failed.
    pub any_failed: bool,
}

impl UpdateReport {
    /// Record a successful or informational component line.
    fn note(&mut self, line: String) {
        self.lines.push(line);
    }

    /// Record a failed component line and mark the report failed.
    fn fail(&mut self, line: String) {
        self.lines.push(line);
        self.any_failed = true;
    }
}

/// Non-interactive update flow for the CLI: desktop app, then the ESPHome
/// package, then the device builder. Invoking the command is the consent, so
/// unlike the tray's Check for Updates arm there are no dialogs; failures are
/// folded into the report instead.
pub(crate) async fn run_full_update(
    app: &AppHandle,
    state: &Arc<AppState>,
    _guard: &UpdateGuard,
    progress: Progress<'_>,
) -> UpdateReport {
    let mut report = UpdateReport {
        app_update_installed: false,
        lines: Vec::new(),
        any_failed: false,
    };

    // Desktop app first: a self-update ships a fresh Python bundle that
    // overwrites the user's `python/` directory, so any pip bump done now
    // would be wiped by the relaunch (same ordering as the tray arm).
    progress("desktop", "checking for a desktop app update");
    match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(update)) => {
                let version = update.version.clone();
                match crate::app_update::apply_update_noninteractive(app, update, progress).await {
                    Ok(()) => {
                        report.note(format!("desktop app updated to {version}"));
                        report.app_update_installed = true;
                    }
                    Err(e) => {
                        // Don't compound a failed self-update with pip activity.
                        report.fail(format!("desktop app update to {version} failed: {e}"));
                    }
                }
                return report;
            }
            Ok(None) => report.note(format!(
                "desktop app {} is up to date",
                app.package_info().version
            )),
            Err(e) => report.fail(format!("desktop app update check failed: {e}")),
        },
        Err(e) => report.fail(format!("desktop updater not available: {e}")),
    }

    let (channel, backend) = {
        let settings = state.settings.read().await;
        (settings.release_channel, settings.backend)
    };

    update_esphome_package(app, state, channel, progress, &mut report).await;
    update_device_builder_package(app, state, backend, progress, &mut report).await;

    report
}

/// ESPHome-package phase of [`run_full_update`].
async fn update_esphome_package(
    app: &AppHandle,
    state: &Arc<AppState>,
    channel: ReleaseChannel,
    progress: Progress<'_>,
    report: &mut UpdateReport,
) {
    progress("esphome", "checking for an ESPHome update");
    let installed = match detect(app, crate::update::get_installed_version).await {
        Ok(v) => v,
        Err(e) => {
            report.fail(format!("esphome version detection failed: {e}"));
            return;
        }
    };

    // The dev channel has no version-based check; `update` reinstalls from the
    // latest dev commit, which is the only way dev moves forward.
    let target = if channel == ReleaseChannel::Dev {
        Some("dev".to_string())
    } else {
        match state.update_checker.check(channel).await {
            Ok(Some(latest)) if crate::update::is_newer_version(&latest, &installed) => {
                Some(latest)
            }
            Ok(_) => None,
            Err(e) => {
                report.fail(format!("esphome update check failed: {e}"));
                return;
            }
        }
    };

    let Some(target) = target else {
        report.note(format!("esphome {installed} is up to date"));
        return;
    };

    let label = if target == "dev" {
        "the latest dev commit".to_string()
    } else {
        target.clone()
    };
    progress(
        "esphome",
        &format!("updating ESPHome {installed} to {label}"),
    );
    let result = stop_install_start(app, state, || async {
        state.update_checker.update_to(app, &target, channel).await
    })
    .await;
    match result {
        Ok(()) => {
            refresh_version_display_blocking(app).await;
            report.note(format!("esphome updated to {label}"));
        }
        Err(e) => report.fail(format!("esphome update failed: {e}")),
    }
}

/// Device-builder phase of [`run_full_update`].
async fn update_device_builder_package(
    app: &AppHandle,
    state: &Arc<AppState>,
    backend: crate::settings::Backend,
    progress: Progress<'_>,
    report: &mut UpdateReport,
) {
    progress("device-builder", "checking for a device builder update");
    let installed = match detect(app, crate::update::get_installed_device_builder_version).await {
        Ok(Some(v)) => v,
        Ok(None) => {
            report.note("device builder is not installed; skipping".to_string());
            return;
        }
        Err(e) => {
            report.fail(format!("device builder version detection failed: {e}"));
            return;
        }
    };

    let latest = match state.update_checker.check_device_builder(backend).await {
        Ok(v) => v,
        Err(e) => {
            report.fail(format!("device builder update check failed: {e}"));
            return;
        }
    };
    if !crate::update::is_newer_version(&latest, &installed) {
        report.note(format!("device builder {installed} is up to date"));
        return;
    }

    progress(
        "device-builder",
        &format!("updating device builder {installed} to {latest}"),
    );
    let result = stop_install_start(app, state, || async {
        state
            .update_checker
            .install_device_builder(app, backend)
            .await
    })
    .await;
    match result {
        Ok(()) => {
            tray::refresh_builder_version_display(app).await;
            report.note(format!("device builder updated to {latest}"));
        }
        Err(e) => report.fail(format!("device builder update failed: {e}")),
    }
}

/// Stop the dashboard, run `install`, then start the dashboard again. The
/// start is attempted even after a failed install so the user isn't left
/// without a dashboard.
async fn stop_install_start<F, Fut>(
    app: &AppHandle,
    state: &Arc<AppState>,
    install: F,
) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    tray::update_status(app, false);
    if let Err(e) = state.daemon.stop().await {
        show_actual_status(app, state);
        return Err(format!("failed to stop the dashboard: {e}"));
    }
    let install_result = install().await;
    let start_result = state.daemon.start().await;
    if start_result.is_ok() {
        tray::update_status(app, true);
    }
    match (install_result, start_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(e)) => Err(format!("updated, but the dashboard failed to start: {e}")),
        (Err(e), Ok(())) => Err(e.to_string()),
        (Err(e), Err(start_err)) => Err(format!(
            "{e}; additionally the dashboard failed to restart: {start_err}"
        )),
    }
}

/// Best-effort dashboard restart after a failed install, so the user isn't
/// left without a backend. Returns whether the restart succeeded.
async fn restart_after_failure(app: &AppHandle, state: &Arc<AppState>, context: &str) -> bool {
    match state.daemon.start().await {
        Ok(()) => {
            tray::update_status(app, true);
            true
        }
        Err(e) => {
            error!("Failed to restart backend after {}: {}", context, e);
            false
        }
    }
}

/// Re-detect the installed ESPHome version and update the tray display, off
/// the async executor (the detection spawns a Python subprocess).
async fn refresh_version_display_blocking(app: &AppHandle) {
    let app = app.clone();
    let _ = tokio::task::spawn_blocking(move || tray::refresh_version_display(&app)).await;
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
