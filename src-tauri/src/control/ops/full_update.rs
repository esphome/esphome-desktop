//! The CLI's non-interactive `update` flow.
//!
//! Desktop app first, then the ESPHome package, then the device builder.
//! Invoking the command is the consent, so unlike the tray's Check for
//! Updates arm there are no dialogs: every failure is folded into the
//! [`UpdateReport`] the client prints.

use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

use super::{stop_install_start, InstallOutcome, Progress, UpdateGuard};
use crate::control::update_check::{
    device_builder_update_available, esphome_install_action, esphome_update_available,
    install_action, InstallAction,
};
use crate::settings::ReleaseChannel;
use crate::{tray, AppState};

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
        state,
        progress,
        report,
        esphome_install_action(check, channel),
        PackagePhase {
            labels: PackageLabels {
                step: "esphome",
                component: "esphome",
                display_name: "ESPHome",
            },
            // The dev "target" is a channel keyword, not a version; quote it
            // as prose in the progress and report lines.
            display_target: |target: &str| {
                if target == "dev" {
                    "the latest dev commit".to_string()
                } else {
                    target.to_string()
                }
            },
            install: |target: String| async move {
                state.update_checker.update_to(app, &target, channel).await
            },
            refresh: || tray::refresh_version_display_blocking(app),
        },
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
        state,
        progress,
        report,
        install_action(check),
        PackagePhase {
            labels: PackageLabels {
                step: "device-builder",
                component: "device builder",
                display_name: "device builder",
            },
            display_target: |latest: &str| latest.to_string(),
            install: |_target| async move {
                state
                    .update_checker
                    .install_device_builder(app, backend)
                    .await
            },
            refresh: || tray::refresh_builder_version_display(app),
        },
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

/// Component-specific configuration for [`run_package_phase`]: the wording
/// [`PackageLabels`], `display_target` mapping the raw install target onto
/// the label quoted in progress and report lines, the `install` future
/// (receives the raw target), and the version-display `refresh` run after a
/// successful install.
struct PackagePhase<D, F, R> {
    labels: PackageLabels,
    display_target: D,
    install: F,
    refresh: R,
}

/// Shared skeleton of the per-package phases of [`run_full_update`]: map the
/// [`InstallAction`] onto the report for the no-op and failure arms, and for
/// an actual install run [`stop_install_start`] and record what it returned.
/// Everything component-specific comes bundled in the [`PackagePhase`] —
/// including the version-display `refresh`, which the install sequence runs
/// itself so the tray and CLI paths refresh at the same point.
async fn run_package_phase<D, F, Fut, R, RFut>(
    state: &Arc<AppState>,
    progress: Progress<'_>,
    report: &mut UpdateReport,
    action: InstallAction,
    phase: PackagePhase<D, F, R>,
) where
    D: FnOnce(&str) -> String,
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = ()>,
{
    let PackagePhase {
        labels,
        display_target,
        install,
        refresh,
    } = phase;
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
    let outcome = stop_install_start(
        state,
        &format!("{component} update"),
        || install(target),
        refresh,
    )
    .await;
    record_outcome(report, component, &label, outcome);
}

/// Record `outcome` as the report line for `component` updating to `label`.
///
/// Split out of [`run_package_phase`] so the wording — which is the CLI's
/// user-facing contract, not an implementation detail — is unit-testable
/// without an `AppState` to feed the surrounding install sequence.
fn record_outcome(
    report: &mut UpdateReport,
    component: &str,
    label: &str,
    outcome: InstallOutcome,
) {
    match outcome {
        InstallOutcome::Installed => report.note(format!("{component} updated to {label}")),
        InstallOutcome::StopFailed(e) => report.fail(format!(
            "{component} update failed: failed to stop the dashboard: {e}"
        )),
        InstallOutcome::StartFailed(e) => report.fail(format!(
            "{component} update failed: updated, but the dashboard failed to start: {e}"
        )),
        InstallOutcome::InstallFailed {
            error,
            restart_error: None,
        } => report.fail(format!("{component} update failed: {error}")),
        InstallOutcome::InstallFailed {
            error,
            restart_error: Some(restart_error),
        } => report.fail(format!(
            "{component} update failed: {error}; additionally the dashboard failed to \
             restart: {restart_error}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The single line `record_outcome` appended, and whether it was recorded
    /// as a failure. Asserting on both together keeps a line that reads like a
    /// failure from being filed as a success.
    fn recorded(outcome: InstallOutcome) -> (String, bool) {
        let mut report = UpdateReport {
            app_update_installed: false,
            lines: Vec::new(),
            any_failed: false,
        };
        record_outcome(&mut report, "esphome", "2026.8.0", outcome);
        assert_eq!(report.lines.len(), 1, "exactly one line per component");
        (report.lines.remove(0), report.any_failed)
    }

    #[test]
    fn installed_reports_the_new_version_without_failing() {
        assert_eq!(
            recorded(InstallOutcome::Installed),
            ("esphome updated to 2026.8.0".to_string(), false)
        );
    }

    #[test]
    fn a_failed_stop_reports_that_nothing_was_installed() {
        let (line, failed) = recorded(InstallOutcome::StopFailed("port busy".into()));
        assert_eq!(
            line,
            "esphome update failed: failed to stop the dashboard: port busy"
        );
        assert!(failed);
    }

    /// The install worked, so the line must say so — a bare "update failed"
    /// would send the user chasing a package that is in fact installed.
    #[test]
    fn a_failed_start_reports_the_install_as_done() {
        let (line, failed) = recorded(InstallOutcome::StartFailed("no such file".into()));
        assert_eq!(
            line,
            "esphome update failed: updated, but the dashboard failed to start: no such file"
        );
        assert!(failed);
    }

    #[test]
    fn a_failed_install_that_restarted_reports_only_the_install_error() {
        let (line, failed) = recorded(InstallOutcome::InstallFailed {
            error: "pip exited 1".into(),
            restart_error: None,
        });
        assert_eq!(line, "esphome update failed: pip exited 1");
        assert!(failed);
    }

    /// Both failures reach the user: the second one means there is no
    /// dashboard running now, which the install error alone would not say.
    #[test]
    fn a_failed_install_that_could_not_restart_reports_both() {
        let (line, failed) = recorded(InstallOutcome::InstallFailed {
            error: "pip exited 1".into(),
            restart_error: Some("no such file".into()),
        });
        assert_eq!(
            line,
            "esphome update failed: pip exited 1; additionally the dashboard failed to \
             restart: no such file"
        );
        assert!(failed);
    }
}
