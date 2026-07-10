//! Update checking functionality
//!
//! Queries PyPI for the latest ESPHome version and notifies the user
//! if an update is available. Supports stable, beta, and dev release channels.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::HashMap;
use tauri::AppHandle;
use tauri_plugin_dialog::MessageDialogKind;
use tauri_plugin_notification::NotificationExt;
use tracing::{debug, error, info, warn};

use crate::control::protocol::channel_name;
use crate::i18n::{t, t_with};
use crate::platform;
use crate::settings::{Backend, ReleaseChannel};

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
            let installed = installed_esphome_version(app_handle).ok().flatten();
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
        let installed = match installed_esphome_version(app_handle) {
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
        let installed = match installed_esphome_version(app_handle) {
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

            // Try a clean --force-reinstall first. If pip aborts because a
            // dependency (e.g. zeroconf) has no RECORD file, retry skipping the
            // uninstall step — same broken-RECORD recovery as #155, here on the
            // dev/GitHub path (#183).
            let pp = python_path.clone();
            install_with_record_retry(
                move |ignore| {
                    let pp = pp.clone();
                    async move { run_dev_install(&pp, ignore).await }
                },
                "ESPHome dev installed successfully from GitHub",
                "ESPHome dev install hit missing RECORD file; retrying with --ignore-installed",
                "pip install from GitHub failed",
            )
            .await
        } else {
            info!("Updating ESPHome to version {}", version);

            let mut cmd = platform::pip_command(&python_path);
            cmd.arg(format!("esphome=={}", version));

            let output = cmd.output().await.context("Failed to run pip install")?;

            if output.status.success() {
                info!("ESPHome updated successfully to {}", version);
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("pip install failed: {}", stderr)
            }
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

        // Try a clean upgrade first (attempts a normal uninstall of the old
        // copy). Only if pip aborts on a missing RECORD file (#155) do we retry
        // with --ignore-installed, which skips the uninstall but orphans stale
        // files — so we limit that trade-off to the broken-RECORD case.
        let pp = python_path.clone();
        let result = install_with_record_retry(
            move |ignore| {
                let pp = pp.clone();
                let version = version.clone();
                async move {
                    run_device_builder_install(&pp, backend, ignore, version.as_deref()).await
                }
            },
            "esphome-device-builder installed/upgraded successfully",
            "esphome-device-builder upgrade hit missing RECORD file; retrying with --ignore-installed",
            "pip install esphome-device-builder failed",
        )
        .await;

        // On success, prune any `.dist-info` dirs the --ignore-installed retry
        // may have orphaned, so the next version check resolves a single, real
        // version instead of looping on "None" (#190). Best-effort.
        if result.is_ok() {
            let _ = dedupe_device_builder_dist_info(app_handle);
        }

        result
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
        let installed = match detect_device_builder_version_with_heal(app_handle) {
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
        let installed = match detect_device_builder_version_with_heal(app_handle) {
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

/// Shared tail of the user-initiated update checks: compare versions and, when
/// `latest` is newer, log it and ask the user whether to update now. Returns
/// `Some(latest)` only when an update is available and the user confirms.
/// When already up to date, logs that at info level and, if
/// `dialog_when_up_to_date` is set, also shows the "No Updates Available"
/// notice (the device-builder flow stays silent; its caller owns that UX).
async fn prompt_if_newer(
    app_handle: &AppHandle,
    wording: &UpdateWording<'_>,
    title: &str,
    latest: String,
    installed: &str,
    dialog_when_up_to_date: bool,
) -> Option<String> {
    if !is_newer_version(&latest, installed) {
        info!("{} is up to date ({})", wording.component, installed);
        if dialog_when_up_to_date {
            crate::dialog::notice(
                app_handle,
                &t("update.none_title"),
                t_with(
                    "update.latest",
                    &[("component", wording.component), ("installed", installed)],
                ),
                MessageDialogKind::Info,
            )
            .await;
        }
        return None;
    }

    info!(
        "{} available: {} -> {} (installed: {})",
        wording.log_prefix, installed, latest, installed
    );

    let msg = wording.prompt_message(&latest, installed);
    if crate::dialog::confirm(
        app_handle,
        title,
        msg,
        &t("common.update_now"),
        &t("common.later"),
    )
    .await
    {
        Some(latest)
    } else {
        None
    }
}

/// Shared tail of the background update checks: compare versions and, when
/// `latest` is newer, log it and show the "<component> Update Available"
/// notification pointing at the updates menu. Logs the up-to-date state at
/// debug level otherwise.
fn notify_if_newer(
    app_handle: &AppHandle,
    wording: &UpdateWording<'_>,
    latest: &str,
    installed: &str,
    tray_available: bool,
) {
    if !is_newer_version(latest, installed) {
        debug!("{} is up to date ({})", wording.component, installed);
        return;
    }

    info!(
        "{} available: {} -> {} (installed: {})",
        wording.log_prefix, installed, latest, installed
    );

    if let Err(e) = notify_update_available(
        app_handle,
        &wording.notification_title(),
        &wording.subject(latest),
        installed,
        tray_available,
    ) {
        error!("Failed to show notification: {}", e);
    }
}

/// Build and show the standard "update available" notification:
/// "<subject> is available (you have <installed>). <updates menu hint>".
/// Returns the show error so each caller keeps its own failure log wording.
pub(crate) fn notify_update_available(
    app_handle: &AppHandle,
    title: &str,
    subject: &str,
    installed: &str,
    tray_available: bool,
) -> tauri_plugin_notification::Result<()> {
    app_handle
        .notification()
        .builder()
        .title(title)
        .body(update_notification_body(subject, installed, tray_available))
        .show()
}

/// Body of the standard "update available" notification, shared by every
/// caller of [`notify_update_available`].
fn update_notification_body(subject: &str, installed: &str, tray_available: bool) -> String {
    t_with(
        "update.notification_body",
        &[
            ("subject", subject),
            ("installed", installed),
            ("hint", &crate::updates_menu_hint(tray_available)),
        ],
    )
}

/// Choose the version to offer on the beta channel.
///
/// Returns the latest beta only when it is strictly newer than the latest
/// stable; otherwise returns `stable`. This prevents a downgrade: after a
/// release cycle closes, the newest beta on PyPI (e.g. "2025.4.0b3") is older
/// than the stable it led to ("2025.4.0"), and `switch_channel(Beta)` installs
/// the returned version unconditionally — without it, a stable user switching
/// to beta would be moved *backwards* onto a stale pre-release.
fn select_beta_target(releases: &HashMap<String, Vec<PyPIRelease>>, stable: &str) -> String {
    match find_latest_beta(releases) {
        Some(beta) if is_newer_version(&beta, stable) => beta,
        _ => stable.to_string(),
    }
}

/// Find the highest version among PyPI releases whose version string matches
/// `predicate`.
///
/// Skips version strings that don't start with a digit (not a valid-looking
/// version) and versions with no installable files (fully yanked or files
/// removed): PyPI keeps the version key with an empty/all-yanked file list,
/// and offering it would download nothing or install a pulled release.
fn highest_version(
    releases: &HashMap<String, Vec<PyPIRelease>>,
    predicate: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut best: Option<String> = None;

    for (version_str, files) in releases {
        if !predicate(version_str) {
            continue;
        }

        // Skip if not a valid-looking version
        if !version_str
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_digit())
        {
            continue;
        }

        // Skip versions with no installable files (fully yanked or removed).
        if !has_active_files(files) {
            continue;
        }

        match &best {
            None => best = Some(version_str.clone()),
            Some(current_best) => {
                if is_newer_version(version_str, current_best) {
                    best = Some(version_str.clone());
                }
            }
        }
    }

    best
}

/// Find the latest beta/pre-release version from PyPI releases.
///
/// Beta versions on PyPI look like "2025.4.0b1", "2025.4.0b2", etc.
/// We find the highest version that contains a beta suffix; ESPHome beta
/// releases always use bN naming.
fn find_latest_beta(releases: &HashMap<String, Vec<PyPIRelease>>) -> Option<String> {
    highest_version(releases, has_beta_suffix)
}

/// Check whether a version string has a beta suffix like "b1", "b2", etc.
/// Matches patterns where a 'b' immediately follows a digit and is followed by
/// one or more digits (e.g. "2025.4.0b1"), which distinguishes it from versions
/// that merely contain the letter 'b' elsewhere.
fn has_beta_suffix(version: &str) -> bool {
    let bytes = version.as_bytes();
    for i in 1..bytes.len().saturating_sub(1) {
        if bytes[i] == b'b' && bytes[i - 1].is_ascii_digit() && bytes[i + 1].is_ascii_digit() {
            return true;
        }
    }
    false
}

/// Whether a release has at least one installable (non-yanked) file.
///
/// A version present in PyPI's `releases` map is not necessarily installable:
/// once every file is yanked or removed, the key lingers with an empty or
/// all-yanked file list. Such a version must not be offered as an update
/// target.
fn has_active_files(files: &[PyPIRelease]) -> bool {
    files.iter().any(|f| !f.yanked)
}

/// Find the highest version across all releases on PyPI, including
/// pre-releases. Used for the "beta" device-builder channel where any
/// pre-release counts (a/b/rc/dev), not just `bN` like ESPHome itself.
fn find_latest_any(releases: &HashMap<String, Vec<PyPIRelease>>) -> Option<String> {
    highest_version(releases, |_| true)
}

/// Maintenance helper run with the bundled interpreter as `python -c <src>
/// <mode>`. `detect` prints the highest installed device-builder version (empty
/// if undeterminable); `dedupe` removes orphaned duplicate `.dist-info` dirs and
/// prints how many it removed. Embedded so it ships with the binary and stays in
/// sync with its pytest suite (`tests/test_device_builder_maintenance.py`).
const DEVICE_BUILDER_MAINT_PY: &str = include_str!("../../scripts/device_builder_maintenance.py");

/// Get the installed `esphome-device-builder` package version.
///
/// - `Ok(Some(v))` — package is installed, returns the version string.
/// - `Ok(None)` — `detect` ran successfully (exit 0) but printed no version: the
///   package is not installed, or duplicate dist-info dirs left it
///   undeterminable (#190).
/// - `Err(e)` — detection itself failed: the bundled Python is missing, the
///   spawn failed, or the helper exited non-zero (a broken interpreter / import
///   error). The caller should surface this rather than treat it as "not
///   installed".
pub fn get_installed_device_builder_version(app_handle: &AppHandle) -> Result<Option<String>> {
    let python_path = platform::get_python_path(app_handle)?;

    // Enumerate all device-builder distributions and take the highest version,
    // which is robust to the duplicate dist-info pileup that makes a plain
    // `importlib.metadata.version(...)` return None or an older version (#190).
    // `-I` (isolated) keeps user site-packages, PYTHONPATH and sitecustomize off
    // sys.path so detection only ever sees the managed bundled install.
    let output = platform::run_python_capture(
        &python_path,
        ["-I", "-c", DEVICE_BUILDER_MAINT_PY, "detect"],
    )
    .context("Failed to run python")?;

    if output.status.success() {
        // `detect` logs skipped/unreadable distributions to stderr; surface it so
        // the reason a version came back undeterminable isn't lost.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            warn!("device-builder version detection: {stderr}");
        }
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // `detect` prints nothing when it cannot determine a version (no install
        // or an unresolvable pileup); the "None" guard is belt-and-suspenders.
        // Treat either as "not determinable" so the updater does not offer an
        // endless update (#190).
        if version.is_empty() || version == "None" {
            return Ok(None);
        }
        Ok(Some(version))
    } else {
        // `detect` exits 0 even when the package is absent (it prints nothing),
        // so a non-zero exit is a real execution failure (broken bundled
        // interpreter, import error, etc.). Surface it rather than silently
        // misclassifying it as "not installed" and skipping the update check.
        anyhow::bail!(
            "device-builder version detection failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
}

/// Remove orphaned duplicate `.dist-info` directories for the device-builder
/// package and its frontend, keeping the highest version's metadata.
///
/// The `--ignore-installed` install fallback (#155/#183) skips the uninstall and
/// leaves the previous version's `.dist-info` behind; once several pile up,
/// `importlib.metadata` can no longer resolve a single version and the updater
/// loops forever offering "version None" (#190). This heals that state. It is
/// best-effort: a failure is logged and swallowed so it can never block an
/// install or an update check.
pub fn dedupe_device_builder_dist_info(app_handle: &AppHandle) -> Result<()> {
    let python_path = platform::get_python_path(app_handle)?;

    // `-I` (isolated) keeps user site-packages, PYTHONPATH and sitecustomize off
    // sys.path so this destructive prune can only ever touch the managed bundled
    // install, never a user-site or externally-injected tree.
    let output = platform::run_python_capture(
        &python_path,
        ["-I", "-c", DEVICE_BUILDER_MAINT_PY, "dedupe"],
    )
    .context("Failed to run dist-info dedup")?;

    if output.status.success() {
        // The helper logs dist-info it couldn't read or remove to stderr;
        // surface it so a partial prune isn't silently lost.
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if !stderr.is_empty() {
            warn!("device-builder dist-info dedup: {stderr}");
        }
        let removed = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !removed.is_empty() && removed != "0" {
            info!("Removed {removed} stale device-builder dist-info dir(s)");
        }
    } else {
        warn!(
            "device-builder dist-info dedup failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Detect the installed device-builder version, healing a duplicate dist-info
/// pileup once if the first lookup cannot determine a version.
///
/// A `None` result is the exact symptom of the pileup (#190), so prune the
/// duplicates and re-query before giving up. A genuinely-absent package stays
/// `None` (dedup finds nothing to remove), at the cost of one extra Python spawn
/// only on the already-unusual not-determinable path.
fn detect_device_builder_version_with_heal(app_handle: &AppHandle) -> Result<Option<String>> {
    let installed = get_installed_device_builder_version(app_handle)?;
    if installed.is_some() {
        return Ok(installed);
    }
    if let Err(e) = dedupe_device_builder_dist_info(app_handle) {
        // The heal is best-effort, but a failed attempt shouldn't be invisible:
        // the re-query below will just return the same undeterminable result.
        warn!("device-builder dist-info heal failed: {e}");
    }
    get_installed_device_builder_version(app_handle)
}

/// Build the `pip install` argument list (appended after the `-m pip install`
/// prefix supplied by [`crate::platform::pip_command`]) for installing/upgrading
/// `esphome-device-builder`.
///
/// When `ignore_installed` is false this is a plain `pip install --upgrade`,
/// which attempts a clean uninstall of the existing copy first — the correct
/// path for normal installs. Pass `true` only as a fallback for the
/// missing-RECORD case (issue #155).
///
/// `--ignore-installed`: the device-builder package ships inside the bundled
/// standalone Python tree, and on some installs its `dist-info/RECORD` is
/// missing. A plain `pip install --upgrade` then tries to uninstall the
/// existing copy first and aborts with `error: uninstall-no-record-file`
/// ("no RECORD file was found"). Retrying with `--ignore-installed` skips the
/// uninstall step and installs the new version over the top, which is pip's own
/// documented recovery for this state.
///
/// Accepted side effect: skipping the uninstall leaves files present in the old
/// version but removed/renamed in the new one orphaned in site-packages. That
/// is why this is a fallback, not the default — it limits the orphaned-files
/// trade-off to the genuinely-broken RECORD case instead of every upgrade.
///
/// `version` pins the package to an exact release (`esphome-device-builder==X`).
/// A plain `--upgrade` never *downgrades*, so switching the device builder from
/// a newer beta to an older stable would otherwise be a silent no-op (#200).
/// Passing the resolved stable version forces pip to install exactly that
/// release, downgrading off the newer beta. Pass `None` to keep the package
/// unpinned (the beta channel, which only ever moves forward).
fn device_builder_install_args(
    backend: Backend,
    ignore_installed: bool,
    version: Option<&str>,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["--upgrade".to_string()];
    if ignore_installed {
        args.push("--ignore-installed".to_string());
    }
    if backend == Backend::BuilderBeta {
        args.push("--pre".to_string());
    }
    match version {
        Some(v) => args.push(format!("esphome-device-builder=={v}")),
        None => args.push("esphome-device-builder".to_string()),
    }
    args
}

/// Run `pip install` for `esphome-device-builder` with the given flags.
async fn run_device_builder_install(
    python_path: &std::path::Path,
    backend: Backend,
    ignore_installed: bool,
    version: Option<&str>,
) -> Result<std::process::Output> {
    let args = device_builder_install_args(backend, ignore_installed, version);
    let mut cmd = platform::pip_command(python_path);
    cmd.args(&args);
    cmd.output().await.context("Failed to run pip install")
}

/// URL of the ESPHome dev-branch source archive installed on the Dev channel.
const ESPHOME_DEV_ZIP_URL: &str = "https://github.com/esphome/esphome/archive/dev.zip";

/// Build the `pip install` argument list (appended after the `-m pip install`
/// prefix supplied by [`crate::platform::pip_command`]) for installing ESPHome from
/// the dev GitHub zip.
///
/// When `ignore_installed` is false this is a plain `--force-reinstall`, which
/// uninstalls the existing copy of each affected package first. Pass `true`
/// only as a fallback for the missing-RECORD case: a bundled dependency such as
/// `zeroconf` can ship without a `dist-info/RECORD` file, and `--force-reinstall`
/// then aborts with `error: uninstall-no-record-file` ("no RECORD file was
/// found", issue #183). `--ignore-installed` skips the uninstall and installs
/// over the top — pip's own documented recovery — at the cost of leaving stale
/// files orphaned, so it is limited to the genuinely-broken RECORD case.
fn dev_install_args(ignore_installed: bool) -> Vec<&'static str> {
    let mut args: Vec<&'static str> = vec!["--force-reinstall"];
    if ignore_installed {
        args.push("--ignore-installed");
    }
    args.push(ESPHOME_DEV_ZIP_URL);
    args
}

/// Run `pip install` for the ESPHome dev GitHub zip with the given flags.
async fn run_dev_install(
    python_path: &std::path::Path,
    ignore_installed: bool,
) -> Result<std::process::Output> {
    let args = dev_install_args(ignore_installed);
    let mut cmd = platform::pip_command(python_path);
    cmd.args(&args);
    cmd.output().await.context("Failed to run pip install")
}

/// Detect pip's missing-RECORD abort, which is the failure that warrants the
/// `--ignore-installed` retry (issue #155).
fn is_missing_record_error(stderr: &str) -> bool {
    stderr.contains("uninstall-no-record-file") || stderr.contains("no RECORD file was found")
}

/// Run a pip install with the shared broken-RECORD recovery policy (#155/#183).
///
/// `run(false)` performs the normal install (a clean uninstall of the old
/// copy). If that aborts because a package has no `dist-info/RECORD` file
/// (`is_missing_record_error`), `run(true)` retries with `--ignore-installed`,
/// which skips the uninstall and installs over the top. Any other failure
/// bails immediately, surfacing the original stderr. Both install paths share
/// this orchestration so the recovery policy lives in one place.
async fn install_with_record_retry<F, Fut>(
    run: F,
    success_msg: &str,
    retry_log: &str,
    fail_prefix: &str,
) -> Result<()>
where
    F: Fn(bool) -> Fut,
    Fut: std::future::Future<Output = Result<std::process::Output>>,
{
    let output = run(false).await?;
    if output.status.success() {
        info!("{success_msg}");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !is_missing_record_error(&stderr) {
        anyhow::bail!("{fail_prefix}: {stderr}");
    }

    info!("{retry_log}");
    let retry = run(true).await?;
    if retry.status.success() {
        info!("{success_msg} (--ignore-installed fallback)");
        Ok(())
    } else {
        anyhow::bail!("{fail_prefix}: {}", String::from_utf8_lossy(&retry.stderr));
    }
}

/// Installed ESPHome version, distinguishing "not installed" from a real
/// detection failure: `Ok(Some(v))` when installed, `Ok(None)` when the
/// `esphome version` command runs but exits non-zero (ESPHome absent), and
/// `Err` only when the check itself can't run (e.g. Python missing). Every
/// caller handles `Ok(None)` explicitly, mirroring the device-builder
/// `get_installed_device_builder_version` shape so "not installed" and
/// "detection failed" never collapse into one state.
pub fn installed_esphome_version(app_handle: &AppHandle) -> Result<Option<String>> {
    let python_path = platform::get_python_path(app_handle)?;

    let Some(version) =
        platform::run_python_capture_stdout(&python_path, ["-m", "esphome", "version"])
            .context("Failed to run esphome version")?
    else {
        return Ok(None);
    };
    // Extract just the version number
    let version = version
        .strip_prefix("Version: ")
        .unwrap_or(&version)
        .to_string();
    Ok(Some(version))
}

/// Pre-release precedence for a version's tag, following PEP 440 ordering:
/// `dev < alpha < beta < rc < release`.
///
/// ESPHome itself only ships `bN` betas (e.g. "2025.4.0b1") and `-dev`
/// builds (e.g. "2026.5.0-dev"), but `esphome-device-builder` is compared
/// with [`find_latest_any`], which can surface any pre-release kind. Ranking
/// them all explicitly avoids mis-selecting an alpha over a beta (both used to
/// share rank 1) or treating a dev build as equal to a beta (both used to be
/// rank 0).
///
/// A bare stable segment never reaches this function — [`parse_version`]
/// assigns it the `255` sentinel directly, so every pre-release tier here
/// sorts below any stable release.
fn prerelease_ord(tag: &str) -> u8 {
    match tag {
        "dev" => 0,
        "a" | "alpha" => 1,
        "b" | "beta" => 2,
        "rc" | "c" | "pre" | "preview" => 3,
        // An unrecognized suffix is treated as the most-final pre-release
        // tier: above every known pre-release but still below a bare stable
        // release. This is conservative — an unexpected tag won't be ranked
        // newer than the stable it precedes.
        _ => 4,
    }
}

/// Re-attach a PEP 440 `.devN` developmental segment to the numeric release
/// segment that precedes it.
///
/// PyPI's JSON API and `importlib.metadata.version()` report developmental
/// releases in normalized PEP 440 form with a **dot** separator
/// (`"2025.5.0.dev3"`), not the hyphenated form (`"2025.5.0-dev"`) the segment
/// parser handles. Without this, `parse_version` splits `"dev3"` off as its own
/// dot-segment, finds no leading digit, and drops it entirely — so the dev
/// build parses identically to the stable `"2025.5.0"`. That silently breaks
/// the device-builder beta channel: a user on one `.devN` build is never
/// notified of a newer `.devN` build of the same base (they compare equal), and
/// `find_latest_any` ranks a dev equal-to-stable / above a beta of the same
/// base, inverting the PEP 440 ordering that [`prerelease_ord`] is meant to
/// enforce.
///
/// Converting `".dev"` → `"-dev"` routes the dev tag through the hyphenated path
/// the tier logic already ranks correctly. Only `.dev` is normalized: among PEP
/// 440 pre-release kinds it is the only one that uses a dot separator (`aN`,
/// `bN`, `rcN` attach directly), so this fully closes the dot-separator gap.
///
/// Returns a borrowed `Cow` when the input has no `.dev` segment (the common
/// case while scanning PyPI releases), allocating only when a substitution is
/// needed. PEP 440 permits at most one `.devN` segment, so only the first
/// occurrence is replaced.
fn normalize_dev_separator(s: &str) -> Cow<'_, str> {
    if s.contains(".dev") {
        Cow::Owned(s.replacen(".dev", "-dev", 1))
    } else {
        Cow::Borrowed(s)
    }
}

/// Parse a version string like "2024.1.0b1", "2026.5.0-dev", or the PEP 440
/// normalized "2026.5.0.dev1" into a comparable representation.
/// Each dot-separated segment becomes (numeric_part, prerelease_order, prerelease_num).
/// A stable segment like "0" becomes (0, 255, 0) so it sorts higher than any pre-release.
fn parse_version(s: &str) -> Vec<(u32, u8, u32)> {
    normalize_dev_separator(s)
        .split('.')
        .filter_map(|part| {
            // Split on pre-release tag boundaries: "0b1", "0-dev"
            // Take the leading digits first
            let num_end = part
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(part.len());
            let numeric: u32 = part[..num_end].parse().ok()?;

            if num_end < part.len() {
                // There's a pre-release suffix
                let suffix = &part[num_end..];
                // Strip a leading hyphen (e.g. "-dev" -> "dev")
                let suffix = suffix.strip_prefix('-').unwrap_or(suffix);
                // Find where the tag name ends and the pre-release number begins
                let tag_end = suffix
                    .find(|c: char| c.is_ascii_digit())
                    .unwrap_or(suffix.len());
                let tag = &suffix[..tag_end];
                let pre_num: u32 = if tag_end < suffix.len() {
                    suffix[tag_end..].parse().unwrap_or(0)
                } else {
                    0
                };
                Some((numeric, prerelease_ord(tag), pre_num))
            } else {
                // Stable segment — sorts higher than any pre-release
                Some((numeric, 255, 0))
            }
        })
        .collect()
}

/// Compare two version strings and return true if `latest` is newer than `installed`
pub(crate) fn is_newer_version(latest: &str, installed: &str) -> bool {
    let latest_parts = parse_version(latest);
    let installed_parts = parse_version(installed);

    // An installed version we cannot parse (e.g. "None", "") must not be treated
    // as infinitely old, or every check would offer an update forever (#190).
    if installed_parts.is_empty() {
        return false;
    }
    // Symmetric: an unparseable "latest" is never newer than a real installed one.
    if latest_parts.is_empty() {
        return false;
    }

    latest_parts > installed_parts
}

#[cfg(test)]
mod tests {
    use super::*;

    /// True if the owned-`String` arg list contains the given flag.
    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    /// The ESPHome wording as built by the check tails (channel label present).
    fn esphome_wording() -> UpdateWording<'static> {
        UpdateWording {
            component: "ESPHome",
            log_prefix: "Update",
            channel_label: Some("stable"),
        }
    }

    #[test]
    fn subject_appends_channel_label_when_present() {
        assert_eq!(
            esphome_wording().subject("2025.1.0"),
            "ESPHome 2025.1.0 (stable)"
        );
        assert_eq!(
            DEVICE_BUILDER_WORDING.subject("1.2.3"),
            "ESPHome Device Builder 1.2.3"
        );
    }

    #[test]
    fn prompt_message_pins_exact_dialog_text() {
        assert_eq!(
            esphome_wording().prompt_message("2025.1.0", "2024.12.2"),
            "ESPHome 2025.1.0 (stable) is available.\n\n\
             You currently have version 2024.12.2.\n\n\
             Would you like to update now?"
        );
        assert_eq!(
            DEVICE_BUILDER_WORDING.prompt_message("1.2.3", "1.2.2"),
            "ESPHome Device Builder 1.2.3 is available.\n\n\
             You currently have version 1.2.2.\n\n\
             Would you like to update now?"
        );
    }

    #[test]
    fn notification_title_pins_exact_text() {
        assert_eq!(
            esphome_wording().notification_title(),
            "ESPHome Update Available"
        );
        assert_eq!(
            DEVICE_BUILDER_WORDING.notification_title(),
            "ESPHome Device Builder Update Available"
        );
    }

    #[test]
    fn notification_body_pins_exact_text_for_both_tray_states() {
        // With a tray, the hint points at the tray menu.
        assert_eq!(
            update_notification_body(&esphome_wording().subject("2025.1.0"), "2024.12.2", true),
            "ESPHome 2025.1.0 (stable) is available (you have 2024.12.2). \
             Open the tray menu and choose \"Check for Updates...\" to update."
        );
        assert_eq!(
            update_notification_body(&DEVICE_BUILDER_WORDING.subject("1.2.3"), "1.2.2", true),
            "ESPHome Device Builder 1.2.3 is available (you have 1.2.2). \
             Open the tray menu and choose \"Check for Updates...\" to update."
        );
        // Without a tray, the hint falls back to the CLI.
        assert_eq!(
            update_notification_body(&esphome_wording().subject("2025.1.0"), "2024.12.2", false),
            "ESPHome 2025.1.0 (stable) is available (you have 2024.12.2). \
             No system tray was detected. Run `esphome-desktop update` from a \
             terminal to update."
        );
        assert_eq!(
            update_notification_body(&DEVICE_BUILDER_WORDING.subject("1.2.3"), "1.2.2", false),
            "ESPHome Device Builder 1.2.3 is available (you have 1.2.2). \
             No system tray was detected. Run `esphome-desktop update` from a \
             terminal to update."
        );
    }

    /// One non-yanked file — a normally installable release.
    fn active() -> Vec<PyPIRelease> {
        vec![PyPIRelease { yanked: false }]
    }

    /// All files yanked — present on PyPI but not installable.
    fn yanked() -> Vec<PyPIRelease> {
        vec![PyPIRelease { yanked: true }]
    }

    #[test]
    fn test_version_comparison() {
        assert!(is_newer_version("2024.2.0", "2024.1.0"));
        assert!(is_newer_version("2024.1.1", "2024.1.0"));
        assert!(is_newer_version("2025.1.0", "2024.12.0"));
        assert!(!is_newer_version("2024.1.0", "2024.1.0"));
        assert!(!is_newer_version("2024.1.0", "2024.2.0"));
        // Stable is newer than beta with same base version
        assert!(is_newer_version("2024.1.0", "2024.1.0b1"));
        // Higher beta number is newer
        assert!(is_newer_version("2024.1.0b2", "2024.1.0b1"));
        // Beta is not newer than stable
        assert!(!is_newer_version("2024.1.0b1", "2024.1.0"));
        // Dev versions use hyphenated suffix: "2026.5.0-dev"
        // Stable is newer than dev with same base version
        assert!(is_newer_version("2026.5.0", "2026.5.0-dev"));
        // Dev is not newer than stable with same base version
        assert!(!is_newer_version("2026.5.0-dev", "2026.5.0"));
        // A newer base version dev is still newer than an older stable
        assert!(is_newer_version("2026.5.0-dev", "2026.4.0"));
    }

    #[test]
    fn test_device_builder_maint_py_well_formed() {
        // Behavior is covered in depth by tests/test_device_builder_maintenance.py;
        // here we only pin the argv-mode contract this module invokes it with, so
        // a rename of the modes (not the internal functions) can't pass silently.
        // Match the bare mode words so the check is tolerant of quote style.
        assert!(DEVICE_BUILDER_MAINT_PY.contains("detect"));
        assert!(DEVICE_BUILDER_MAINT_PY.contains("dedupe"));
        assert!(DEVICE_BUILDER_MAINT_PY.contains("esphome-device-builder-frontend"));
    }

    #[test]
    fn test_unparseable_installed_version_is_not_offered_an_update() {
        // Regression for #190: duplicate dist-info dirs make the version lookup
        // return "None"/"", which must never be treated as infinitely old.
        assert!(!is_newer_version("1.0.10", "None"));
        assert!(!is_newer_version("1.0.10", ""));
        assert!(!is_newer_version("2025.5.0", "None"));
        // An unparseable "latest" is never newer than a real installed version.
        assert!(!is_newer_version("None", "1.0.10"));
        // Sanity: real comparisons still work.
        assert!(is_newer_version("1.0.10", "1.0.9"));
        assert!(!is_newer_version("1.0.10", "1.0.10"));
    }

    #[test]
    fn test_prerelease_precedence_ordering() {
        // PEP 440 ordering within the same base version:
        //   dev < alpha < beta < rc < release
        assert!(is_newer_version("2025.4.0a1", "2025.4.0-dev"));
        assert!(is_newer_version("2025.4.0b1", "2025.4.0a1"));
        assert!(is_newer_version("2025.4.0rc1", "2025.4.0b1"));
        assert!(is_newer_version("2025.4.0", "2025.4.0rc1"));

        // Transitivity check across the full chain.
        assert!(is_newer_version("2025.4.0rc1", "2025.4.0-dev"));
        assert!(is_newer_version("2025.4.0b1", "2025.4.0-dev"));

        // Long-form tags rank identically to their short forms.
        assert!(is_newer_version("2025.4.0beta1", "2025.4.0alpha1"));
        assert!(!is_newer_version("2025.4.0alpha2", "2025.4.0beta1"));

        // A dev build is no longer considered equal to a beta of the same
        // base (they previously both mapped to rank 0).
        assert!(is_newer_version("2025.4.0b1", "2025.4.0-dev"));
        assert!(!is_newer_version("2025.4.0-dev", "2025.4.0b1"));

        // "c" is an accepted alias for "rc".
        assert!(is_newer_version("2025.4.0c1", "2025.4.0b9"));
    }

    #[test]
    fn test_pep440_dot_dev_separator() {
        // PyPI / importlib.metadata report dev releases with a dot separator.
        // These must rank identically to the hyphenated form.

        // Stable is newer than a dot-form dev of the same base.
        assert!(is_newer_version("2025.5.0", "2025.5.0.dev3"));
        assert!(!is_newer_version("2025.5.0.dev3", "2025.5.0"));

        // A newer dev build of the same base is detected (the bug: both used to
        // collapse to the stable representation and compare equal).
        assert!(is_newer_version("2025.5.0.dev5", "2025.5.0.dev3"));
        assert!(!is_newer_version("2025.5.0.dev3", "2025.5.0.dev5"));

        // Dot-form dev sorts below a beta/rc of the same base (PEP 440 order).
        assert!(is_newer_version("2025.5.0b1", "2025.5.0.dev9"));
        assert!(is_newer_version("2025.5.0rc1", "2025.5.0.dev9"));

        // Dot and hyphen forms of the same dev build are equivalent.
        assert!(!is_newer_version("2025.5.0.dev3", "2025.5.0-dev3"));
        assert!(!is_newer_version("2025.5.0-dev3", "2025.5.0.dev3"));

        // A newer base version dev is still newer than an older stable.
        assert!(is_newer_version("2025.6.0.dev1", "2025.5.0"));
    }

    #[test]
    fn test_device_builder_install_args_default_no_ignore_installed() {
        // The default (first-attempt) upgrade must NOT pass --ignore-installed,
        // so normal installs still get a clean uninstall of the old copy.
        for backend in [Backend::BuilderStable, Backend::BuilderBeta] {
            let args = device_builder_install_args(backend, false, None);
            assert!(!has(&args, "--ignore-installed"), "backend {backend:?}");
            assert!(has(&args, "--upgrade"), "backend {backend:?}");
            assert_eq!(
                args.last().map(String::as_str),
                Some("esphome-device-builder")
            );
        }
    }

    #[test]
    fn test_device_builder_install_args_ignore_installed_fallback() {
        // The fallback path adds --ignore-installed so a missing RECORD file in
        // the bundled install can't abort the retry (issue #155).
        for backend in [Backend::BuilderStable, Backend::BuilderBeta] {
            let args = device_builder_install_args(backend, true, None);
            assert!(has(&args, "--ignore-installed"), "backend {backend:?}");
            assert!(has(&args, "--upgrade"), "backend {backend:?}");
            assert_eq!(
                args.last().map(String::as_str),
                Some("esphome-device-builder")
            );
        }
    }

    #[test]
    fn test_device_builder_install_args_pre_only_for_beta() {
        for ignore_installed in [false, true] {
            assert!(has(
                &device_builder_install_args(Backend::BuilderBeta, ignore_installed, None),
                "--pre"
            ));
            assert!(!has(
                &device_builder_install_args(Backend::BuilderStable, ignore_installed, None),
                "--pre"
            ));
        }
    }

    #[test]
    fn test_device_builder_install_args_pins_version_for_downgrade() {
        // The #200 fix: passing an explicit version pins the package to that
        // exact release (`==X`). A plain `--upgrade` never downgrades, so the
        // pin is what forces pip off a newer installed beta onto the older
        // stable when switching channels.
        for ignore_installed in [false, true] {
            let args = device_builder_install_args(
                Backend::BuilderStable,
                ignore_installed,
                Some("1.2.3"),
            );
            assert!(has(&args, "--upgrade"));
            assert!(!has(&args, "--pre"));
            assert_eq!(
                args.last().map(String::as_str),
                Some("esphome-device-builder==1.2.3")
            );
        }
    }

    #[test]
    fn test_is_missing_record_error() {
        assert!(is_missing_record_error("error: uninstall-no-record-file"));
        assert!(is_missing_record_error(
            "Cannot uninstall esphome-device-builder ...: no RECORD file was found"
        ));
        assert!(!is_missing_record_error("some other pip failure"));
    }

    #[test]
    fn test_is_missing_record_error_dev_zeroconf() {
        // The #183 dev-channel failure: a dependency (zeroconf) lacks a RECORD
        // file, which must also trigger the --ignore-installed retry.
        assert!(is_missing_record_error(
            "error: uninstall-no-record-file\n\n× Cannot uninstall zeroconf None\n╰─> The package's contents are unknown: no RECORD file was found for zeroconf."
        ));
    }

    #[test]
    fn test_dev_install_args_default_no_ignore_installed() {
        // The default (first-attempt) dev install must NOT pass
        // --ignore-installed, so a normal install still reinstalls cleanly.
        let args = dev_install_args(false);
        assert!(!args.contains(&"--ignore-installed"));
        assert!(args.contains(&"--force-reinstall"));
        assert_eq!(args.last(), Some(&ESPHOME_DEV_ZIP_URL));
    }

    #[test]
    fn test_dev_install_args_ignore_installed_fallback() {
        // The fallback adds --ignore-installed so a missing RECORD file in a
        // bundled dependency can't abort the retry (issue #183).
        let args = dev_install_args(true);
        assert!(args.contains(&"--ignore-installed"));
        assert!(args.contains(&"--force-reinstall"));
        assert_eq!(args.last(), Some(&ESPHOME_DEV_ZIP_URL));
    }

    /// Build a canned `Output` with the given success flag and stderr, so the
    /// retry orchestration can be unit-tested without spawning pip.
    fn fake_output(success: bool, stderr: &str) -> std::process::Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            // Unix wait-status: 0 is success; exit code 1 encodes as 1 << 8.
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 })
        };
        std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn test_install_with_record_retry_success_first_try() {
        // A clean install never triggers the --ignore-installed retry.
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
        let seen = calls.clone();
        let result = install_with_record_retry(
            move |ignore| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(ignore);
                    Ok(fake_output(true, ""))
                }
            },
            "ok",
            "retrying",
            "failed",
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(*calls.lock().unwrap(), vec![false]);
    }

    #[tokio::test]
    async fn test_install_with_record_retry_recovers_on_missing_record() {
        // First attempt aborts on a missing RECORD file; the helper retries
        // with ignore_installed=true and succeeds (issues #155/#183).
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
        let seen = calls.clone();
        let result = install_with_record_retry(
            move |ignore| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(ignore);
                    if ignore {
                        Ok(fake_output(true, ""))
                    } else {
                        Ok(fake_output(false, "error: uninstall-no-record-file"))
                    }
                }
            },
            "ok",
            "retrying",
            "failed",
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(*calls.lock().unwrap(), vec![false, true]);
    }

    #[tokio::test]
    async fn test_install_with_record_retry_bails_on_other_failure() {
        // A failure that is NOT a missing-RECORD abort bails immediately,
        // surfacing the original stderr without retrying.
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<bool>::new()));
        let seen = calls.clone();
        let result = install_with_record_retry(
            move |ignore| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(ignore);
                    Ok(fake_output(false, "some other pip failure"))
                }
            },
            "ok",
            "retrying",
            "pip blew up",
        )
        .await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("pip blew up"));
        assert!(err.contains("some other pip failure"));
        assert_eq!(*calls.lock().unwrap(), vec![false]);
    }

    #[test]
    fn test_has_beta_suffix() {
        assert!(has_beta_suffix("2025.4.0b1"));
        assert!(has_beta_suffix("2025.4.0b12"));
        assert!(!has_beta_suffix("2025.4.0"));
        assert!(!has_beta_suffix("2025.4.0-dev"));
        // Should not match 'b' that isn't a digit-b-digit pattern
        assert!(!has_beta_suffix("abc"));
    }

    #[test]
    fn test_find_latest_beta() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), active());
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), active());
        releases.insert("2025.3.0b1".to_string(), active());

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b2".to_string()));
    }

    #[test]
    fn test_find_latest_beta_none() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), active());
        releases.insert("2025.4.0".to_string(), active());

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, None);
    }

    #[test]
    fn test_find_latest_beta_skips_yanked() {
        // The newest beta on PyPI was yanked — fall back to the next
        // installable beta rather than offering the pulled release.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), yanked());

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b1".to_string()));
    }

    #[test]
    fn test_find_latest_beta_skips_empty_file_list() {
        // A version key with no files (all removed from PyPI) is not
        // installable and must be ignored.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), vec![]);

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b1".to_string()));
    }

    #[test]
    fn test_find_latest_any_skips_yanked() {
        let mut releases = HashMap::new();
        releases.insert("2025.4.0".to_string(), active());
        releases.insert("2025.5.0b1".to_string(), yanked());

        // The only newer candidate is yanked, so the highest installable
        // version wins.
        assert_eq!(find_latest_any(&releases), Some("2025.4.0".to_string()));
    }

    #[test]
    fn test_select_beta_target_prefers_newer_beta() {
        // A beta for the next release exists and is newer than stable.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0".to_string(), active());
        releases.insert("2025.5.0b1".to_string(), active());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.5.0b1".to_string()
        );
    }

    #[test]
    fn test_select_beta_target_avoids_downgrade_to_old_beta() {
        // The release cycle finished: the newest beta on PyPI is the
        // pre-release that led to the current stable. Offering it would
        // downgrade a beta-channel user — fall back to stable instead.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0b1".to_string(), active());
        releases.insert("2025.4.0b2".to_string(), active());
        releases.insert("2025.4.0".to_string(), active());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.4.0".to_string()
        );
    }

    #[test]
    fn test_select_beta_target_falls_back_when_newest_beta_yanked() {
        // The next-cycle beta exists but was yanked: don't offer it, fall
        // back to the current stable instead of an uninstallable release.
        let mut releases = HashMap::new();
        releases.insert("2025.4.0".to_string(), active());
        releases.insert("2025.5.0b1".to_string(), yanked());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.4.0".to_string()
        );
    }

    #[test]
    fn test_select_beta_target_no_beta_uses_stable() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), active());
        releases.insert("2025.4.0".to_string(), active());

        assert_eq!(
            select_beta_target(&releases, "2025.4.0"),
            "2025.4.0".to_string()
        );
    }
}
