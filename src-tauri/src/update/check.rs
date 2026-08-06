//! Asking what is available: PyPI queries and the check tails they feed.
//!
//! Everything here only reads — it asks PyPI which versions exist and asks the
//! user whether they want one. Acting on the answer (installing, switching
//! channel, repairing the tree) is [`super::apply`].

use anyhow::{Context, Result};
use tauri::AppHandle;
use tauri_plugin_dialog::MessageDialogKind;
use tracing::{debug, info, warn};

use crate::i18n::{t, t_with};
use crate::settings::{Backend, ReleaseChannel};

use super::install::{
    detect_device_builder_version_with_heal_async, installed_esphome_version_async, HealOutcome,
    HealPolicy,
};
use super::notify::{notify_if_newer, prompt_if_newer};
use super::version::{find_latest_any, select_beta_target};
use super::{PyPIResponse, UpdateChecker, UpdateWording, DEVICE_BUILDER_WORDING};

impl UpdateChecker {
    /// Fetch and parse the PyPI JSON metadata for `package`.
    ///
    /// Callers pass fixed internal package names, so no URL encoding is needed.
    async fn fetch_pypi(&self, package: &str) -> Result<PyPIResponse> {
        self.client
            .get(format!("https://pypi.org/pypi/{package}/json"))
            .send()
            .await
            .with_context(|| format!("Failed to fetch PyPI info for {package}"))?
            .json()
            .await
            .with_context(|| format!("Failed to parse PyPI response for {package}"))
    }

    /// Check for updates and return the latest version string for the given channel.
    ///
    /// - Stable: returns the latest stable version from PyPI
    /// - Beta: returns the latest pre-release (beta) version from PyPI
    /// - Dev: always returns None (dev channel doesn't do version-based updates)
    pub async fn check(&self, channel: ReleaseChannel) -> Result<Option<String>> {
        match channel {
            ReleaseChannel::Stable => {
                debug!("Checking for stable ESPHome updates on PyPI");
                let response = self.fetch_pypi("esphome").await?;

                let latest = response.info.version;
                info!("Latest stable ESPHome version on PyPI: {}", latest);
                Ok(Some(latest))
            }
            ReleaseChannel::Beta => {
                debug!("Checking for beta ESPHome updates on PyPI");
                let response = self.fetch_pypi("esphome").await?;

                // Pick the version to offer on the beta channel. We want the
                // newest beta (e.g. "2025.4.0b1"), but only when it is actually
                // newer than the latest stable. Once a release cycle finishes
                // and the stable ships, the newest *beta* on PyPI is an older
                // pre-release — offering it would downgrade a beta-channel user
                // (switch_channel installs unconditionally, with no is_newer
                // guard). In that case fall back to stable.
                let target = select_beta_target(&response.releases, &response.info.version);
                info!("Beta channel target ESPHome version: {}", target);
                Ok(Some(target))
            }
            ReleaseChannel::Dev => {
                // Dev channel doesn't use version-based update checks
                debug!("Dev channel: skipping version-based update check");
                Ok(None)
            }
        }
    }

    /// Check for updates (user-initiated) - always shows feedback via dialog
    /// Returns Some(version) if user wants to update, None otherwise
    pub async fn check_for_user(
        &self,
        app_handle: &AppHandle,
        channel: ReleaseChannel,
    ) -> Option<String> {
        // Dev channel: offer to reinstall from git HEAD
        if channel == ReleaseChannel::Dev {
            let installed = installed_esphome_version_async(app_handle)
                .await
                .ok()
                .flatten();
            let unknown = t("version.unknown");
            let installed_str = installed.as_deref().unwrap_or(&unknown);

            let should_update = crate::dialog::confirm(
                app_handle,
                &t("update.dev_channel_title"),
                t_with("update.dev_channel_prompt", &[("version", installed_str)]),
                &t("common.update_now"),
                &t("common.cancel"),
            )
            .await;

            if should_update {
                // Return a sentinel value that update_to will recognize
                return Some("dev".to_string());
            }
            return None;
        }

        // Get installed version
        let installed = match installed_esphome_version_async(app_handle).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                crate::dialog::notice(
                    app_handle,
                    &t("update.check_failed_title"),
                    t("update.not_installed"),
                    MessageDialogKind::Error,
                )
                .await;
                return None;
            }
            Err(e) => {
                warn!("Could not detect installed version: {}", e);
                crate::dialog::notice(
                    app_handle,
                    &t("update.check_failed_title"),
                    t_with("update.detect_failed", &[("error", &e.to_string())]),
                    MessageDialogKind::Error,
                )
                .await;
                return None;
            }
        };

        // Check for latest version
        let latest = match self.check(channel).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                crate::dialog::notice(
                    app_handle,
                    &t("update.check_failed_title"),
                    t("update.latest_unknown"),
                    MessageDialogKind::Error,
                )
                .await;
                return None;
            }
            Err(e) => {
                warn!("Update check failed: {}", e);
                crate::dialog::notice(
                    app_handle,
                    &t("update.check_failed_title"),
                    t_with("update.check_failed", &[("error", &e.to_string())]),
                    MessageDialogKind::Error,
                )
                .await;
                return None;
            }
        };

        // Compare versions and ask the user. Dev is handled at the top of this
        // function, so the constructor's dev-channel panic is unreachable here.
        prompt_if_newer(
            app_handle,
            &UpdateWording::esphome(channel),
            &t("update.available_title"),
            latest,
            &installed,
            true,
        )
        .await
    }

    /// Check for updates and notify the user if one is available (background check).
    /// Does nothing for the dev channel.
    pub async fn check_and_notify(
        &self,
        app_handle: &AppHandle,
        channel: ReleaseChannel,
        tray_available: bool,
    ) {
        if channel == ReleaseChannel::Dev {
            debug!("Dev channel: skipping background update check");
            return;
        }

        // Get installed version
        let installed = match installed_esphome_version_async(app_handle).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                debug!("ESPHome not installed; skipping update notification");
                return;
            }
            Err(e) => {
                warn!("Could not detect installed version: {}", e);
                return;
            }
        };

        // Check for latest version
        let latest = match self.check(channel).await {
            Ok(Some(v)) => v,
            Ok(None) => return,
            Err(e) => {
                warn!("Update check failed: {}", e);
                return;
            }
        };

        // Compare versions and notify. Dev is handled at the top of this
        // function, so the constructor's dev-channel panic is unreachable here.
        notify_if_newer(
            app_handle,
            &UpdateWording::esphome(channel),
            &latest,
            &installed,
            tray_available,
        );
    }

    /// Query PyPI for the latest available `esphome-device-builder` version.
    /// `Backend::BuilderStable` returns the latest final release; `BuilderBeta`
    /// returns the latest version including pre-releases.
    pub async fn check_device_builder(&self, backend: Backend) -> Result<String> {
        let response = self.fetch_pypi("esphome-device-builder").await?;

        let include_pre = backend == Backend::BuilderBeta;
        let latest = if include_pre {
            find_latest_any(&response.releases).unwrap_or(response.info.version)
        } else {
            response.info.version
        };
        info!(
            "Latest esphome-device-builder version on PyPI ({}): {}",
            backend, latest
        );
        Ok(latest)
    }

    /// Background check for esphome-device-builder updates. Emits a
    /// notification if a newer version is available.
    pub async fn check_and_notify_device_builder(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
        tray_available: bool,
    ) {
        // The daily background check runs without the UpdateGuard; the heal
        // takes the guard itself for its duration, or defers if a sequence
        // is in flight (see HealPolicy).
        let installed =
            match detect_device_builder_version_with_heal_async(app_handle, HealPolicy::WhenIdle)
                .await
            {
                Ok(HealOutcome::Determined(Some(v))) => v,
                Ok(HealOutcome::Determined(None)) => {
                    debug!("esphome-device-builder is not installed; skipping update check");
                    return;
                }
                Ok(HealOutcome::Deferred) => {
                    // Says nothing about the package — the version was
                    // undeterminable while a sequence held the guard, which
                    // is most likely pip mid-install. The next daily check
                    // heals a real pileup.
                    debug!(
                        "device-builder version undeterminable while an update sequence \
                         is in flight; skipping update check"
                    );
                    return;
                }
                Err(e) => {
                    warn!("esphome-device-builder version detection failed: {}", e);
                    return;
                }
            };

        let latest = match self.check_device_builder(backend).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Device-builder update check failed: {}", e);
                return;
            }
        };

        notify_if_newer(
            app_handle,
            &DEVICE_BUILDER_WORDING,
            &latest,
            &installed,
            tray_available,
        );
    }

    /// User-initiated check for esphome-device-builder updates. Returns
    /// `Some(version)` if the user wants to update, `None` otherwise.
    /// Stays silent when there is no update — the caller is responsible
    /// for the "everything is up to date" UX.
    ///
    /// `guard` is the caller's held `UpdateGuard`: proof that its sequence is
    /// the only one running, which is what lets the dist-info heal run here
    /// (see `HealPolicy`). This path is the main user-facing recovery for the
    /// #190 pileup, so the heal must not stand down on it.
    pub(crate) async fn check_device_builder_for_user(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
        guard: &crate::control::ops::UpdateGuard,
    ) -> Option<String> {
        let installed = match detect_device_builder_version_with_heal_async(
            app_handle,
            HealPolicy::GuardHeld(guard),
        )
        .await
        {
            Ok(HealOutcome::Determined(Some(v))) => v,
            Ok(HealOutcome::Determined(None)) => {
                warn!("esphome-device-builder is not installed");
                return None;
            }
            Ok(HealOutcome::Deferred) => {
                // Unreachable under GuardHeld — a held guard is never denied
                // a permit — but if it ever fires, silence is the safe UX.
                warn!("device-builder heal deferred under a held guard; treating as undetermined");
                return None;
            }
            Err(e) => {
                warn!(
                    "Could not detect installed esphome-device-builder version: {}",
                    e
                );
                return None;
            }
        };

        let latest = match self.check_device_builder(backend).await {
            Ok(v) => v,
            Err(e) => {
                warn!("Device-builder update check failed: {}", e);
                return None;
            }
        };

        prompt_if_newer(
            app_handle,
            &DEVICE_BUILDER_WORDING,
            &t("update.builder_available_title"),
            latest,
            &installed,
            false,
        )
        .await
    }
}
