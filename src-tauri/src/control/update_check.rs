//! Update-availability checks and the install-action decision.
//!
//! The passive "is a newer version available?" queries behind the dashboard's
//! update banner and the CLI `status` reply, plus the pure mapping from a
//! [`ComponentUpdate`] onto what an install sequence should do
//! ([`install_action`]). Keeping the decision here — separate from the
//! stop→install→start *sequences* in [`super::ops`] — keeps it side-effect-free
//! and unit-testable, and guarantees the install path derives "is an update
//! available" from the exact value the check path reports.

use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_updater::UpdaterExt;

use super::protocol::ComponentUpdate;
use crate::settings::ReleaseChannel;
use crate::AppState;

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
pub(super) enum InstallAction {
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
pub(super) fn install_action(check: ComponentUpdate) -> InstallAction {
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
pub(super) fn esphome_install_action(
    check: ComponentUpdate,
    channel: ReleaseChannel,
) -> InstallAction {
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

#[cfg(test)]
mod tests {
    use super::*;

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
