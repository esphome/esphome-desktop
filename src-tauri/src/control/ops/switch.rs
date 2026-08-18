//! Channel / backend switch flows and the dashboard restart.
//!
//! The stop→install→persist→start sequences behind the tray's Switch Channel
//! and Switch Backend radio arms and their CLI equivalents, plus the plain
//! Restart Dashboard. All three report through a [`Progress`] sink; the two
//! switches hand back a [`SwitchOutcome`] the surfaces render themselves,
//! while the restart reports whether the dashboard came back.

use std::sync::Arc;
use tauri::AppHandle;
use tracing::{error, info};

use super::{restart_after_failure, set_and_save, Progress, UpdateGuard, READY_TIMEOUT_SECS};
use crate::settings::ReleaseChannel;
use crate::{tray, AppState};

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

/// Stop prologue shared by the switch flows: report progress and stop the
/// daemon (which reflects the stop in the tray status line itself). On
/// failure run `revert` (restores the tray radio checks) and hand back the
/// [`SwitchOutcome::StopFailed`] for the caller to return. `stop_what` names
/// what failed to stop in the log line.
async fn stop_or_revert(
    state: &Arc<AppState>,
    progress: Progress<'_>,
    detail: &str,
    stop_what: &str,
    revert: impl FnOnce(),
) -> Result<(), SwitchOutcome> {
    progress("stop", detail);
    if let Err(e) = state.daemon.stop().await {
        error!("Failed to stop {}: {}", stop_what, e);
        revert();
        return Err(SwitchOutcome::StopFailed(e.to_string()));
    }
    Ok(())
}

/// Install-failure epilogue shared by the switch flows: run `revert` to
/// restore the tray radio checks, then attempt a best-effort restart of the
/// previous install (`context` feeds the restart-failure log), folding both
/// into [`SwitchOutcome::InstallFailed`]. Callers log their flow-specific
/// error line before calling.
async fn install_failed(
    state: &Arc<AppState>,
    error: String,
    context: &str,
    revert: impl FnOnce(),
) -> SwitchOutcome {
    revert();
    let restarted = restart_after_failure(state, context).await;
    SwitchOutcome::InstallFailed { error, restarted }
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

    if let Err(outcome) = stop_or_revert(
        state,
        progress,
        "stopping the dashboard",
        "backend for channel switch",
        || tray::update_channel_checks(old_channel),
    )
    .await
    {
        return outcome;
    }

    progress(
        "install",
        &format!("installing ESPHome from the {} channel", new_channel),
    );
    match state.update_checker.switch_channel(app, new_channel).await {
        Ok(()) => {
            info!("Switched to {} channel successfully", new_channel);

            set_and_save(app, state, |settings| {
                settings.release_channel = new_channel;
                true
            })
            .await;

            tray::refresh_version_display_blocking(app).await;

            progress("start", "starting the dashboard");
            if let Err(e) = state.daemon.start().await {
                error!("Failed to restart backend after channel switch: {}", e);
                return SwitchOutcome::StartFailed(e.to_string());
            }
            SwitchOutcome::Success { ready: true }
        }
        Err(e) => {
            error!("Channel switch failed: {}", e);
            install_failed(state, e.to_string(), "failed channel switch", || {
                tray::update_channel_checks(old_channel)
            })
            .await
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

    if let Err(outcome) = stop_or_revert(
        state,
        progress,
        "stopping the backend",
        "daemon for backend switch",
        || tray::update_backend_checks(old_backend),
    )
    .await
    {
        return outcome;
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
        return install_failed(state, e.to_string(), "failed backend switch", || {
            tray::update_backend_checks(old_backend)
        })
        .await;
    }
    // Install succeeded — refresh the tray version display.
    tray::refresh_builder_version_display(app).await;

    // Persist the new backend channel.
    set_and_save(app, state, |settings| {
        settings.backend = new_backend;
        true
    })
    .await;

    progress("start", "starting the backend");
    if let Err(e) = state.daemon.start().await {
        error!("Failed to start daemon after backend switch: {}", e);
        return SwitchOutcome::StartFailed(e.to_string());
    }
    info!("Switched backend to {}", new_backend);

    progress("wait", "waiting for the backend to become ready");
    let port = state.daemon.port();
    let ready = crate::wait_for_dashboard_ready(port, READY_TIMEOUT_SECS).await;
    SwitchOutcome::Success { ready }
}

/// Restart the dashboard backend. With `wait_ready` the call also polls the
/// dashboard's readiness probe and returns whether it came up within 60s.
pub(crate) async fn restart_daemon(
    state: &Arc<AppState>,
    wait_ready: bool,
    _guard: &UpdateGuard,
    progress: Progress<'_>,
) -> Result<bool, String> {
    progress("restart", "restarting the dashboard");
    if let Err(e) = state.daemon.restart().await {
        return Err(e.to_string());
    }
    if !wait_ready {
        return Ok(true);
    }
    progress("wait", "waiting for the dashboard to become ready");
    Ok(crate::wait_for_dashboard_ready(state.daemon.port(), READY_TIMEOUT_SECS).await)
}
