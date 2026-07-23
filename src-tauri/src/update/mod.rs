//! Update checking functionality
//!
//! Queries PyPI for the latest ESPHome version and notifies the user
//! if an update is available. Supports stable, beta, and dev release channels.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use tauri::AppHandle;
use tauri_plugin_dialog::MessageDialogKind;
use tracing::{debug, info, warn};

use crate::control::protocol::channel_name;
use crate::i18n::{t, t_with};
use crate::platform;
use crate::settings::{Backend, ReleaseChannel};

mod install;
mod notify;
mod version;

pub use install::{
    dedupe_device_builder_dist_info, get_installed_device_builder_version,
    installed_esphome_version,
};
pub(crate) use notify::notify_update_available;
pub(crate) use version::is_newer_version;

use install::{
    detect_device_builder_version_with_heal_async, install_with_record_recovery,
    installed_esphome_version_async, interpreter_usable, notify_repair_incomplete,
    notify_repair_needed, probe_esphome, repair_hint, run_dev_install, run_device_builder_install,
    run_esphome_install,
};
use notify::{notify_if_newer, prompt_if_newer};
use version::{find_latest_any, select_beta_target};

/// PyPI package info response (used for stable channel)
#[derive(Debug, Deserialize)]
struct PyPIResponse {
    info: PyPIInfo,
    releases: HashMap<String, Vec<PyPIRelease>>,
}

#[derive(Debug, Deserialize)]
struct PyPIInfo {
    version: String,
}

/// A single release file entry from PyPI. We only need to know whether the
/// file has been yanked — PyPI keeps a version's key in `releases` even after
/// every file is yanked or removed, so a lingering entry does not mean the
/// version is actually installable.
#[derive(Debug, Deserialize)]
struct PyPIRelease {
    #[serde(default)]
    yanked: bool,
}

/// Update checker
pub struct UpdateChecker {
    client: reqwest::Client,
}

impl UpdateChecker {
    /// Create a new update checker
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
        }
    }

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

        // Compare versions and ask the user. Dev is handled at the top of
        // this function, so channel_name only ever yields "stable" or "beta";
        // keep that invariant explicit.
        let channel_label = match channel {
            ReleaseChannel::Stable | ReleaseChannel::Beta => channel_name(channel),
            ReleaseChannel::Dev => {
                unreachable!("dev channel is handled before the shared check tail")
            }
        };
        prompt_if_newer(
            app_handle,
            &UpdateWording {
                component: "ESPHome",
                log_prefix: "Update",
                channel_label: Some(channel_label),
            },
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
        // function, so channel_name only ever yields "stable" or "beta";
        // keep that invariant explicit.
        let channel_label = match channel {
            ReleaseChannel::Stable | ReleaseChannel::Beta => channel_name(channel),
            ReleaseChannel::Dev => {
                unreachable!("dev channel is handled before the shared check tail")
            }
        };
        notify_if_newer(
            app_handle,
            &UpdateWording {
                component: "ESPHome",
                log_prefix: "Update",
                channel_label: Some(channel_label),
            },
            &latest,
            &installed,
            tray_available,
        );
    }

    /// Perform an update to the specified version, or install from git for dev channel.
    pub async fn update_to(
        &self,
        app_handle: &AppHandle,
        version: &str,
        channel: ReleaseChannel,
    ) -> Result<()> {
        let python_path = platform::get_python_path(app_handle)?;

        if channel == ReleaseChannel::Dev || version == "dev" {
            info!("Installing ESPHome from GitHub (dev channel)");

            // A clean --force-reinstall. If pip aborts because a dependency
            // (e.g. zeroconf) has no RECORD file, repair the tree and retry
            // against a clean copy — same broken-RECORD recovery as #155, here
            // on the dev/GitHub path (#183).
            let pp = python_path.clone();
            install_with_record_recovery(
                move || {
                    let pp = pp.clone();
                    async move { run_dev_install(&pp).await }
                },
                || self.repair_python_tree(app_handle),
                "ESPHome dev installed successfully from GitHub",
                "pip install from GitHub failed",
            )
            .await
        } else {
            info!("Updating ESPHome to version {}", version);

            // Pin the exact version and route through the shared broken-RECORD
            // recovery. A stable/beta `pip install esphome==X` uninstalls the
            // differing installed copy first, and that uninstall aborts with
            // `error: uninstall-no-record-file` when the bundled tree has a
            // missing `dist-info/RECORD` — the same failure the dev (#183) and
            // device-builder (#155) paths already recover from. Without this,
            // stable/beta was the one install path lacking that parity.
            let pp = python_path.clone();
            let version = version.to_string();
            install_with_record_recovery(
                move || {
                    let pp = pp.clone();
                    let version = version.clone();
                    async move { run_esphome_install(&pp, &version).await }
                },
                || self.repair_python_tree(app_handle),
                "ESPHome updated successfully",
                "pip install failed",
            )
            .await
        }
    }

    /// Install or upgrade the `esphome-device-builder` package from PyPI.
    /// Pass `Backend::BuilderBeta` to allow pre-releases (`pip install --pre`),
    /// `Backend::BuilderStable` for stable-only.
    pub async fn install_device_builder(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
    ) -> Result<()> {
        let python_path = platform::get_python_path(app_handle)?;

        info!("Installing/upgrading esphome-device-builder ({})", backend);

        // For the stable channel, resolve the concrete latest stable version and
        // pin it so pip will *downgrade* off a newer installed beta. A plain
        // `--upgrade` without a pin never downgrades, so a beta->stable switch
        // would otherwise be a silent no-op (#200). Beta stays unpinned
        // (`--pre --upgrade`), which already moves forward to the latest
        // pre-release. If the PyPI lookup fails we fall back to the unpinned
        // upgrade rather than block the install entirely.
        let version = if backend == Backend::BuilderStable {
            match self.check_device_builder(Backend::BuilderStable).await {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        "Could not resolve latest stable device-builder version; \
                         falling back to unpinned upgrade (may not downgrade a beta): {}",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        // A clean upgrade, which uninstalls the old copy normally. Only if pip
        // aborts on a missing RECORD file (#155) do we repair the tree and
        // retry against a clean copy.
        let pp = python_path.clone();
        install_with_record_recovery(
            move || {
                let pp = pp.clone();
                let version = version.clone();
                async move { run_device_builder_install(&pp, backend, version.as_deref()).await }
            },
            || self.repair_python_tree(app_handle),
            "esphome-device-builder installed/upgraded successfully",
            "pip install esphome-device-builder failed",
        )
        .await
    }

    /// Repair a broken managed Python tree by re-copying the bundled one.
    ///
    /// Every platform ships a pristine copy of the tree inside the app and
    /// keeps a working copy in app data (#335), so the one repair everywhere is
    /// a local file copy: free, offline, and the same path that already heals
    /// the tree at every release.
    async fn repair_python_tree(&self, app_handle: &AppHandle) -> Result<()> {
        info!("Repairing the ESPHome install by re-copying the bundled Python tree");
        let app = app_handle.clone();
        tokio::task::spawn_blocking(move || {
            platform::ensure_user_python(&app, platform::RefreshReason::Repair)
        })
        .await
        .context("Bundled-Python refresh task panicked or was cancelled")?
    }

    /// Check the bundled tree with a real ESPHome command at startup and repair
    /// it if it is broken (#330).
    ///
    /// This exists because the damage outlives the bug that caused it. Removing
    /// the `--ignore-installed` fallback stops us orphaning files from now on,
    /// but every user it already hit still has the orphan on disk, and nothing
    /// else would ever clear it: their next update can succeed and still leave
    /// the stale directory sitting there breaking every compile. So look for the
    /// damage directly rather than waiting for an install to fail.
    ///
    /// Never blocks the launch: a probe that cannot run, an exhausted attempt
    /// budget, or a failed repair all continue to start the app. But a tree left
    /// broken is never silent — every compile will fail, and the user is the only
    /// one who can act on it, so [`notify_repair_needed`] tells them. Must run
    /// before the daemon starts: a running backend holds the packages open, and
    /// it would be serving a broken tree anyway.
    pub async fn repair_python_tree_if_broken(&self, app_handle: &AppHandle) {
        let python_path = match platform::get_python_path(app_handle) {
            Ok(p) => p,
            Err(e) => {
                warn!("Skipping ESPHome health probe; no Python found: {e:#}");
                return;
            }
        };

        // `get_python_path` falls back to a bare system `python3` in development
        // builds with no bundle. That interpreter fails the probe simply because
        // ESPHome is not installed in it, which is not damage and not ours to
        // repair; probing it would only produce a notification telling a
        // developer their install is broken.
        if !platform::is_managed_python_tree(&python_path) {
            debug!("Skipping ESPHome health probe; {python_path:?} is not a managed tree");
            return;
        }

        let python_parent_dir = match platform::get_python_parent_dir(app_handle) {
            Ok(d) => d,
            Err(e) => {
                warn!("Skipping ESPHome health probe; no local data dir: {e:#}");
                return;
            }
        };

        let detail = match probe_esphome(&python_path).await {
            Ok(None) => {
                debug!("ESPHome health probe passed");
                platform::clear_repair_count(&python_parent_dir);
                return;
            }
            Ok(Some(detail)) => detail,
            // The probe could not run at all. That does NOT mean the interpreter
            // is broken: the probe also needs a writable temp dir and somewhere
            // to put a config, so a full disk fails it just as well. Ask the
            // interpreter directly rather than inferring, on every platform —
            // acting on the inference would either delete a working tree we
            // cannot re-copy onto a full disk, or tell a user their install is
            // damaged when it is their disk. Both are worse than doing nothing.
            //
            // `interpreter_is_usable` is the right question to ask because it
            // spawns nothing but the interpreter, so none of those environment
            // failures can reach it.
            //
            // Deliberately not left to `ensure_user_python`'s own
            // `interpreter_is_usable` wipe: that only runs when `needs_copy` is
            // true, so a tree that broke without an app update keeps a matching
            // marker and is never reached.
            Err(e) => {
                match interpreter_usable(&python_path).await {
                    Ok(true) => {
                        warn!(
                            "ESPHome health probe could not run, but the interpreter itself is \
                             fine, so this is the environment rather than a tree we can repair: \
                             {e:#}"
                        );
                        return;
                    }
                    // Nothing established that the interpreter is fine, so do not
                    // act as though it had — in either direction.
                    Err(join) => {
                        warn!(
                            "Could not check whether the interpreter is usable ({join}), so the \
                             tree is being left alone. The probe said: {e:#}"
                        );
                        return;
                    }
                    Ok(false) => {}
                }

                // The interpreter really is wedged. A bundle re-copy fixes that
                // and needs nothing from the broken one, so fall through to the
                // repair.
                format!("the interpreter could not run the health probe: {e:#}")
            }
        };

        if !platform::may_repair_tree(&python_parent_dir) {
            warn!(
                "ESPHome install looks broken but the repair budget is spent, so it is being \
                 left alone. Probe said: {detail}"
            );
            // Not `repair_budget_left`: we were just refused, and an
            // unwritable counter refuses while still reading under the bound.
            notify_repair_needed(
                app_handle,
                t_with(
                    "update.repair_incomplete",
                    &[("hint", &repair_hint(&python_parent_dir, false))],
                ),
            );
            return;
        }

        warn!("ESPHome install is broken; repairing it. Probe said: {detail}");
        if let Err(e) = self.repair_python_tree(app_handle).await {
            warn!("ESPHome repair failed: {e:#}");
            notify_repair_needed(
                app_handle,
                t_with(
                    "update.repair_failed",
                    &[
                        ("error", &e.to_string()),
                        (
                            "hint",
                            &repair_hint(
                                &python_parent_dir,
                                platform::repair_budget_left(&python_parent_dir),
                            ),
                        ),
                    ],
                ),
            );
            return;
        }

        // Confirm the repair with the same probe that condemned the tree, so a
        // repair that did not actually fix anything says so rather than being
        // reported as a success.
        match probe_esphome(&python_path).await {
            Ok(None) => {
                info!("ESPHome install repaired");
                platform::clear_repair_count(&python_parent_dir);
            }
            Ok(Some(detail)) => {
                warn!("ESPHome install still broken after the repair: {detail}");
                notify_repair_incomplete(app_handle, &python_parent_dir);
            }
            // The repair ran but we could not confirm it. Treat that as
            // unrepaired rather than as success: the probe already proved the
            // tree was broken, so "we cannot tell" is much closer to "still
            // broken" than to "fine", and staying quiet here would make an
            // unverifiable repair the one outcome the user is never told about.
            // Leaving the counter alone is deliberate for the same reason — an
            // unconfirmed repair has not earned back its budget.
            Err(e) => {
                warn!("Could not re-check the ESPHome install after the repair: {e:#}");
                notify_repair_incomplete(app_handle, &python_parent_dir);
            }
        }
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
        let installed = match detect_device_builder_version_with_heal_async(app_handle).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                debug!("esphome-device-builder is not installed; skipping update check");
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
    pub async fn check_device_builder_for_user(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
    ) -> Option<String> {
        let installed = match detect_device_builder_version_with_heal_async(app_handle).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                warn!("esphome-device-builder is not installed");
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

    /// Switch to a new release channel by installing the appropriate version.
    /// Returns Ok(()) on success.
    pub async fn switch_channel(
        &self,
        app_handle: &AppHandle,
        channel: ReleaseChannel,
    ) -> Result<()> {
        match channel {
            ReleaseChannel::Stable => {
                // Install the latest stable version
                let latest = self
                    .check(ReleaseChannel::Stable)
                    .await?
                    .context("Could not determine latest stable version")?;
                self.update_to(app_handle, &latest, ReleaseChannel::Stable)
                    .await
            }
            ReleaseChannel::Beta => {
                // Install the latest beta version
                let latest = self
                    .check(ReleaseChannel::Beta)
                    .await?
                    .context("Could not determine latest beta version")?;
                self.update_to(app_handle, &latest, ReleaseChannel::Beta)
                    .await
            }
            ReleaseChannel::Dev => {
                // Install from GitHub
                self.update_to(app_handle, "dev", ReleaseChannel::Dev).await
            }
        }
    }
}

/// Per-component wording shared by the user-prompt and background-notify
/// update-check tails, so the strings cannot drift between the two flows.
struct UpdateWording<'a> {
    /// Component display name, e.g. "ESPHome" or "ESPHome Device Builder".
    component: &'a str,
    /// Leading words of the "<log_prefix> available: a -> b" info log.
    log_prefix: &'a str,
    /// Release-channel label appended to the offered version, when shown.
    channel_label: Option<&'a str>,
}

/// Wording for the `esphome-device-builder` check tails (no channel label;
/// the backend channel is implied by which backend is configured).
const DEVICE_BUILDER_WORDING: UpdateWording<'static> = UpdateWording {
    component: "ESPHome Device Builder",
    log_prefix: "Device-builder update",
    channel_label: None,
};

impl UpdateWording<'_> {
    /// "<component> <version>" with the channel label appended when present,
    /// e.g. "ESPHome 2025.1.0 (stable)" or "ESPHome Device Builder 1.2.3".
    fn subject(&self, version: &str) -> String {
        match self.channel_label {
            Some(label) => format!("{} {} ({})", self.component, version, label),
            None => format!("{} {}", self.component, version),
        }
    }

    /// Full body of the "would you like to update now?" confirm dialog shown
    /// by [`prompt_if_newer`].
    fn prompt_message(&self, latest: &str, installed: &str) -> String {
        t_with(
            "update.available_prompt",
            &[("subject", &self.subject(latest)), ("installed", installed)],
        )
    }

    /// Title of the background "update available" notification shown by
    /// [`notify_if_newer`].
    fn notification_title(&self) -> String {
        t_with(
            "update.notification_title",
            &[("component", self.component)],
        )
    }
}
