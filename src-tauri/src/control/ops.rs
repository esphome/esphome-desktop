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

use super::protocol::ComponentUpdate;
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

/// Detect a component's installed version via [`detect`], mapping the two
/// non-detected outcomes onto the [`ComponentUpdate`] early exits the check
/// replies carry: absent is a normal state ("nothing to update"), reported as
/// `not_installed()` rather than an error, and a detection failure is
/// `errored` with `installed: None` — the encoding [`install_action`] relies
/// on to tell detection failures from update-check failures.
async fn detect_installed_or_report<F>(app: &AppHandle, f: F) -> Result<String, ComponentUpdate>
where
    F: FnOnce(&AppHandle) -> anyhow::Result<Option<String>> + Send + 'static,
{
    match detect(app, f).await {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(ComponentUpdate::not_installed()),
        Err(e) => Err(ComponentUpdate::errored(None, e)),
    }
}

/// Whether `latest` is a newer version than `installed`, mapped onto the
/// [`ComponentUpdate`] the check reply carries. The install sequences derive
/// their decision from this same result via [`install_action`], so on the
/// stable and beta channels the `available` flag never disagrees with what an
/// actual `update` installs. The one deliberate exception is the ESPHome dev
/// channel, where the check reports "current" but `update` always reinstalls
/// (see [`esphome_install_action`]).
fn compare(installed: String, latest: String) -> ComponentUpdate {
    if crate::update::is_newer_version(&latest, &installed) {
        ComponentUpdate::upgradable(installed, latest)
    } else {
        ComponentUpdate::current(installed, latest)
    }
}

/// Whether a desktop app self-update is available, without installing it. Reads
/// the same `updater().check()` the update flow uses, but only reports.
pub(crate) async fn desktop_update_available(app: &AppHandle) -> ComponentUpdate {
    let installed = app.package_info().version.to_string();
    match app.updater() {
        Ok(updater) => match updater.check().await {
            Ok(Some(update)) => ComponentUpdate::upgradable(installed, update.version),
            Ok(None) => ComponentUpdate::current(installed.clone(), installed),
            Err(e) => ComponentUpdate::errored(Some(installed), e.to_string()),
        },
        Err(e) => ComponentUpdate::errored(Some(installed), e.to_string()),
    }
}

/// Whether an ESPHome package update is available, without installing it.
/// Also the source of truth for the install path (see [`install_action`]).
pub(crate) async fn esphome_update_available(
    app: &AppHandle,
    state: &Arc<AppState>,
    channel: ReleaseChannel,
) -> ComponentUpdate {
    let installed =
        match detect_installed_or_report(app, crate::update::installed_esphome_version).await {
            Ok(v) => v,
            Err(early) => return early,
        };
    // The dev channel has no version-based check: `update` always reinstalls the
    // latest dev commit, so a passive check can't call it "newer". Report it as
    // current so the dashboard banner doesn't nag on every dev build.
    if channel == ReleaseChannel::Dev {
        return ComponentUpdate::current(installed.clone(), installed);
    }
    match state.update_checker.check(channel).await {
        Ok(Some(latest)) => compare(installed, latest),
        Ok(None) => ComponentUpdate::current(installed.clone(), installed),
        Err(e) => ComponentUpdate::errored(Some(installed), e.to_string()),
    }
}

/// Whether a device-builder package update is available, without installing
/// it. Also the source of truth for the install path (see [`install_action`]).
pub(crate) async fn device_builder_update_available(
    app: &AppHandle,
    state: &Arc<AppState>,
    backend: crate::settings::Backend,
) -> ComponentUpdate {
    let installed =
        match detect_installed_or_report(app, crate::update::get_installed_device_builder_version)
            .await
        {
            Ok(v) => v,
            Err(early) => return early,
        };
    match state.update_checker.check_device_builder(backend).await {
        Ok(latest) => compare(installed, latest),
        Err(e) => ComponentUpdate::errored(Some(installed), e.to_string()),
    }
}

/// What an install sequence should do, decided purely from a
/// [`ComponentUpdate`] produced by the availability helpers. Keeping the
/// decision out of the async install fns makes it unit-testable and keeps the
/// install path's view of "is an update available" derived from the exact
/// value the check path reports; the only deliberate divergence is the
/// dev-channel override in [`esphome_install_action`].
#[derive(Debug, PartialEq)]
enum InstallAction {
    /// Install `target` over `installed`.
    Install { installed: String, target: String },
    /// Nothing newer exists.
    UpToDate { installed: String },
    /// The component is not installed; skip it.
    NotInstalled,
    /// Version detection failed (the availability helpers encode this as
    /// `errored` with `installed: None` — they return before ever running the
    /// update check when detection fails).
    DetectionFailed(String),
    /// Detection succeeded but the update check failed (`errored` with
    /// `installed: Some(_)`).
    CheckFailed(String),
}

/// Map an availability result onto the install action. Shared by the ESPHome
/// and device-builder phases; the dev-channel override is layered on top in
/// [`esphome_install_action`].
fn install_action(check: ComponentUpdate) -> InstallAction {
    match check {
        ComponentUpdate {
            error: Some(e),
            installed: None,
            ..
        } => InstallAction::DetectionFailed(e),
        ComponentUpdate { error: Some(e), .. } => InstallAction::CheckFailed(e),
        ComponentUpdate {
            installed: None, ..
        } => InstallAction::NotInstalled,
        ComponentUpdate {
            available: true,
            installed: Some(installed),
            latest: Some(target),
            ..
        } => InstallAction::Install { installed, target },
        ComponentUpdate {
            available,
            installed: Some(installed),
            ..
        } => {
            // `available` without a `latest` is unconstructible today
            // (`upgradable` is the only constructor that sets it, and it
            // always carries both versions). Assert that so a future
            // constructor can't silently downgrade an "available" result to
            // up to date; in release builds the conservative reading (never
            // install on incomplete data) stands.
            debug_assert!(
                !available,
                "ComponentUpdate available without a latest version"
            );
            InstallAction::UpToDate { installed }
        }
    }
}

/// ESPHome install action: [`install_action`] plus the dev-channel rule. The
/// dev channel has no version-based check — the availability helper reports
/// dev as "current" so the dashboard banner doesn't nag, but `update` always
/// reinstalls the latest dev commit, which is the only way dev moves forward.
/// Rewriting only `UpToDate` keeps the not-installed and error paths intact.
fn esphome_install_action(check: ComponentUpdate, channel: ReleaseChannel) -> InstallAction {
    match install_action(check) {
        InstallAction::UpToDate { installed } if channel == ReleaseChannel::Dev => {
            InstallAction::Install {
                installed,
                target: "dev".to_string(),
            }
        }
        action => action,
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
    let check = esphome_update_available(app, state, channel).await;
    run_package_phase(
        app,
        state,
        progress,
        report,
        PackageLabels {
            step: "esphome",
            component: "esphome",
            display_name: "ESPHome",
        },
        esphome_install_action(check, channel),
        // The dev "target" is a channel keyword, not a version; quote it as
        // prose in the progress and report lines.
        |target| {
            if target == "dev" {
                "the latest dev commit".to_string()
            } else {
                target.to_string()
            }
        },
        |target| async move { state.update_checker.update_to(app, &target, channel).await },
        || refresh_version_display_blocking(app),
    )
    .await;
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
    let check = device_builder_update_available(app, state, backend).await;
    run_package_phase(
        app,
        state,
        progress,
        report,
        PackageLabels {
            step: "device-builder",
            component: "device builder",
            display_name: "device builder",
        },
        install_action(check),
        |latest| latest.to_string(),
        |_target| async move {
            state
                .update_checker
                .install_device_builder(app, backend)
                .await
        },
        || tray::refresh_builder_version_display(app),
    )
    .await;
}

/// Wording knobs for [`run_package_phase`]: the progress `step` key, the
/// lowercase `component` noun the report lines start with, and the
/// `display_name` quoted in the "updating …" progress detail.
struct PackageLabels {
    step: &'static str,
    component: &'static str,
    display_name: &'static str,
}

/// Shared skeleton of the per-package phases of [`run_full_update`]: map the
/// [`InstallAction`] onto the report for the no-op and failure arms, and for
/// an actual install run [`stop_install_start`], refresh the version display
/// on success, and record the outcome. `display_target` maps the raw install
/// target onto the label quoted in progress and report lines; `install`
/// receives the raw target.
#[allow(clippy::too_many_arguments)]
async fn run_package_phase<F, Fut, R, RFut>(
    app: &AppHandle,
    state: &Arc<AppState>,
    progress: Progress<'_>,
    report: &mut UpdateReport,
    labels: PackageLabels,
    action: InstallAction,
    display_target: impl FnOnce(&str) -> String,
    install: F,
    refresh: R,
) where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = ()>,
{
    let component = labels.component;
    let (installed, target) = match action {
        InstallAction::Install { installed, target } => (installed, target),
        InstallAction::UpToDate { installed } => {
            report.note(format!("{component} {installed} is up to date"));
            return;
        }
        InstallAction::NotInstalled => {
            report.note(format!("{component} is not installed; skipping"));
            return;
        }
        InstallAction::DetectionFailed(e) => {
            report.fail(format!("{component} version detection failed: {e}"));
            return;
        }
        InstallAction::CheckFailed(e) => {
            report.fail(format!("{component} update check failed: {e}"));
            return;
        }
    };

    let label = display_target(&target);
    progress(
        labels.step,
        &format!("updating {} {installed} to {label}", labels.display_name),
    );
    let result = stop_install_start(app, state, || install(target)).await;
    match result {
        Ok(()) => {
            refresh().await;
            report.note(format!("{component} updated to {label}"));
        }
        Err(e) => report.fail(format!("{component} update failed: {e}")),
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

    #[test]
    fn install_action_upgradable_installs_latest() {
        let action = install_action(ComponentUpdate::upgradable(
            "2025.6.0".into(),
            "2025.7.0".into(),
        ));
        assert_eq!(
            action,
            InstallAction::Install {
                installed: "2025.6.0".into(),
                target: "2025.7.0".into(),
            }
        );
    }

    #[test]
    fn install_action_current_is_up_to_date() {
        let action = install_action(ComponentUpdate::current(
            "2025.7.0".into(),
            "2025.7.0".into(),
        ));
        assert_eq!(
            action,
            InstallAction::UpToDate {
                installed: "2025.7.0".into(),
            }
        );
    }

    #[test]
    fn install_action_not_installed_skips() {
        assert_eq!(
            install_action(ComponentUpdate::not_installed()),
            InstallAction::NotInstalled
        );
    }

    #[test]
    fn install_action_maps_detection_failure() {
        assert_eq!(
            install_action(ComponentUpdate::errored(None, "python broke".into())),
            InstallAction::DetectionFailed("python broke".into())
        );
    }

    #[test]
    fn install_action_maps_check_failure() {
        assert_eq!(
            install_action(ComponentUpdate::errored(
                Some("1.2.3".into()),
                "network down".into()
            )),
            InstallAction::CheckFailed("network down".into())
        );
    }

    #[test]
    fn esphome_dev_channel_always_reinstalls() {
        let action = esphome_install_action(
            ComponentUpdate::current("2026.7.0-dev".into(), "2026.7.0-dev".into()),
            ReleaseChannel::Dev,
        );
        assert_eq!(
            action,
            InstallAction::Install {
                installed: "2026.7.0-dev".into(),
                target: "dev".into(),
            }
        );
    }

    #[test]
    fn esphome_dev_channel_not_installed_skips() {
        assert_eq!(
            esphome_install_action(ComponentUpdate::not_installed(), ReleaseChannel::Dev),
            InstallAction::NotInstalled
        );
    }

    #[test]
    fn esphome_dev_channel_detection_failure_still_fails() {
        assert_eq!(
            esphome_install_action(
                ComponentUpdate::errored(None, "boom".into()),
                ReleaseChannel::Dev
            ),
            InstallAction::DetectionFailed("boom".into())
        );
    }

    #[test]
    fn esphome_stable_channel_uses_plain_mapping() {
        let action = esphome_install_action(
            ComponentUpdate::upgradable("2025.6.0".into(), "2025.7.0".into()),
            ReleaseChannel::Stable,
        );
        assert_eq!(
            action,
            InstallAction::Install {
                installed: "2025.6.0".into(),
                target: "2025.7.0".into(),
            }
        );
        assert_eq!(
            esphome_install_action(
                ComponentUpdate::current("2025.7.0".into(), "2025.7.0".into()),
                ReleaseChannel::Stable,
            ),
            InstallAction::UpToDate {
                installed: "2025.7.0".into(),
            }
        );
    }
}
