//! Update checking functionality
//!
//! Queries PyPI for the latest ESPHome version and notifies the user
//! if an update is available. Supports stable, beta, and dev release channels.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tracing::{debug, error, info, warn};

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

/// A single release file entry from PyPI (we only need it to check existence)
#[derive(Debug, Deserialize)]
struct PyPIRelease {}

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

    /// Check for updates and return the latest version string for the given channel.
    ///
    /// - Stable: returns the latest stable version from PyPI
    /// - Beta: returns the latest pre-release (beta) version from PyPI
    /// - Dev: always returns None (dev channel doesn't do version-based updates)
    pub async fn check(&self, channel: ReleaseChannel) -> Result<Option<String>> {
        match channel {
            ReleaseChannel::Stable => {
                debug!("Checking for stable ESPHome updates on PyPI");
                let response: PyPIResponse = self
                    .client
                    .get("https://pypi.org/pypi/esphome/json")
                    .send()
                    .await
                    .context("Failed to fetch PyPI info")?
                    .json()
                    .await
                    .context("Failed to parse PyPI response")?;

                let latest = response.info.version;
                info!("Latest stable ESPHome version on PyPI: {}", latest);
                Ok(Some(latest))
            }
            ReleaseChannel::Beta => {
                debug!("Checking for beta ESPHome updates on PyPI");
                let response: PyPIResponse = self
                    .client
                    .get("https://pypi.org/pypi/esphome/json")
                    .send()
                    .await
                    .context("Failed to fetch PyPI info")?
                    .json()
                    .await
                    .context("Failed to parse PyPI response")?;

                // Find the latest beta/pre-release version from all releases.
                // Beta versions contain 'b' (e.g., "2025.4.0b1").
                // We want the newest version that is a pre-release, or fall
                // back to the latest stable if no beta is newer.
                let latest_beta = find_latest_beta(&response.releases);

                match latest_beta {
                    Some(v) => {
                        info!("Latest beta ESPHome version on PyPI: {}", v);
                        Ok(Some(v))
                    }
                    None => {
                        // No beta found that's newer; fall back to stable
                        let stable = &response.info.version;
                        info!(
                            "No beta version found newer than stable ({}), using stable",
                            stable
                        );
                        Ok(Some(stable.clone()))
                    }
                }
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
            let installed = get_installed_version(app_handle).ok();
            let installed_str = installed
                .as_deref()
                .unwrap_or("unknown")
                .to_string();

            let dialog_app = app_handle.clone();
            let should_update = tokio::task::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message(format!(
                        "You are on the dev channel.\n\n\
                         Currently installed: {}\n\n\
                         This will reinstall ESPHome from the latest commit on GitHub.\n\n\
                         Would you like to update now?",
                        installed_str
                    ))
                    .title("Dev Channel Update")
                    .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                        "Update Now".to_string(),
                        "Cancel".to_string(),
                    ))
                    .blocking_show()
            })
            .await
            .unwrap_or(false);

            if should_update {
                // Return a sentinel value that update_to will recognize
                return Some("dev".to_string());
            }
            return None;
        }

        // Get installed version
        let installed = match get_installed_version(app_handle) {
            Ok(v) => v,
            Err(e) => {
                warn!("Could not detect installed version: {}", e);
                let dialog_app = app_handle.clone();
                let msg = format!("Could not detect installed version: {}", e);
                let _ = tokio::task::spawn_blocking(move || {
                    dialog_app
                        .dialog()
                        .message(msg)
                        .kind(MessageDialogKind::Error)
                        .title("Update Check Failed")
                        .blocking_show();
                })
                .await;
                return None;
            }
        };

        // Check for latest version
        let latest = match self.check(channel).await {
            Ok(Some(v)) => v,
            Ok(None) => {
                let dialog_app = app_handle.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    dialog_app
                        .dialog()
                        .message("Could not determine latest version")
                        .kind(MessageDialogKind::Error)
                        .title("Update Check Failed")
                        .blocking_show();
                })
                .await;
                return None;
            }
            Err(e) => {
                warn!("Update check failed: {}", e);
                let dialog_app = app_handle.clone();
                let msg = format!("Failed to check for updates: {}", e);
                let _ = tokio::task::spawn_blocking(move || {
                    dialog_app
                        .dialog()
                        .message(msg)
                        .kind(MessageDialogKind::Error)
                        .title("Update Check Failed")
                        .blocking_show();
                })
                .await;
                return None;
            }
        };

        // Compare versions
        if is_newer_version(&latest, &installed) {
            info!(
                "Update available: {} -> {} (installed: {})",
                installed, latest, installed
            );

            let channel_label = match channel {
                ReleaseChannel::Stable => "stable",
                ReleaseChannel::Beta => "beta",
                ReleaseChannel::Dev => unreachable!(),
            };

            // Ask user if they want to update
            let dialog_app = app_handle.clone();
            let msg = format!(
                "ESPHome {} ({}) is available.\n\nYou currently have version {}.\n\nWould you like to update now?",
                latest, channel_label, installed
            );
            let should_update = tokio::task::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message(msg)
                    .title("Update Available")
                    .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                        "Update Now".to_string(),
                        "Later".to_string(),
                    ))
                    .blocking_show()
            })
            .await
            .unwrap_or(false);

            if should_update {
                return Some(latest);
            }
        } else {
            info!("ESPHome is up to date ({})", installed);

            let dialog_app = app_handle.clone();
            let msg = format!("ESPHome {} is the latest version.", installed);
            let _ = tokio::task::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message(msg)
                    .kind(MessageDialogKind::Info)
                    .title("No Updates Available")
                    .blocking_show();
            })
            .await;
        }

        None
    }

    /// Check for updates and notify the user if one is available (background check).
    /// Does nothing for the dev channel.
    pub async fn check_and_notify(&self, app_handle: &AppHandle, channel: ReleaseChannel) {
        if channel == ReleaseChannel::Dev {
            debug!("Dev channel: skipping background update check");
            return;
        }

        // Get installed version
        let installed = match get_installed_version(app_handle) {
            Ok(v) => v,
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

        // Compare versions
        if is_newer_version(&latest, &installed) {
            info!(
                "Update available: {} -> {} (installed: {})",
                installed, latest, installed
            );

            let channel_label = match channel {
                ReleaseChannel::Stable => "stable",
                ReleaseChannel::Beta => "beta",
                ReleaseChannel::Dev => unreachable!(),
            };

            // Show notification
            if let Err(e) = app_handle
                .notification()
                .builder()
                .title("ESPHome Update Available")
                .body(format!(
                    "ESPHome {} ({}) is available (you have {}). Click 'Check for Updates' in the menu to update.",
                    latest, channel_label, installed
                ))
                .show()
            {
                error!("Failed to show notification: {}", e);
            }
        } else {
            debug!("ESPHome is up to date ({})", installed);
        }
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

            let mut cmd = tokio::process::Command::new(&python_path);
            cmd.args([
                "-m",
                "pip",
                "install",
                "--force-reinstall",
                "https://github.com/esphome/esphome/archive/dev.zip",
            ]);
            platform::configure_no_window_tokio_command(&mut cmd);

            let output = cmd.output().await.context("Failed to run pip install")?;

            if output.status.success() {
                info!("ESPHome dev installed successfully from GitHub");
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("pip install from GitHub failed: {}", stderr)
            }
        } else {
            info!("Updating ESPHome to version {}", version);

            let mut cmd = tokio::process::Command::new(&python_path);
            cmd.args(["-m", "pip", "install", &format!("esphome=={}", version)]);
            platform::configure_no_window_tokio_command(&mut cmd);

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
    /// `Backend::BuilderStable` for stable-only. Calling with `Backend::Classic`
    /// is a no-op.
    pub async fn install_device_builder(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
    ) -> Result<()> {
        if !backend.is_builder() {
            return Ok(());
        }
        let python_path = platform::get_python_path(app_handle)?;

        info!("Installing/upgrading esphome-device-builder ({})", backend);

        let mut args: Vec<&str> = vec!["-m", "pip", "install", "--upgrade"];
        if backend == Backend::BuilderBeta {
            args.push("--pre");
        }
        args.push("esphome-device-builder");

        let mut cmd = tokio::process::Command::new(&python_path);
        cmd.args(&args);
        platform::configure_no_window_tokio_command(&mut cmd);

        let output = cmd.output().await.context("Failed to run pip install")?;

        if output.status.success() {
            info!("esphome-device-builder installed/upgraded successfully");
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("pip install esphome-device-builder failed: {}", stderr)
        }
    }

    /// Query PyPI for the latest available `esphome-device-builder` version.
    /// `Backend::BuilderStable` returns the latest final release; `BuilderBeta`
    /// returns the latest version including pre-releases. Returns `Ok(None)`
    /// if the backend is not a builder variant.
    pub async fn check_device_builder(&self, backend: Backend) -> Result<Option<String>> {
        if !backend.is_builder() {
            return Ok(None);
        }
        let response: PyPIResponse = self
            .client
            .get("https://pypi.org/pypi/esphome-device-builder/json")
            .send()
            .await
            .context("Failed to fetch PyPI info for esphome-device-builder")?
            .json()
            .await
            .context("Failed to parse PyPI response for esphome-device-builder")?;

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
        Ok(Some(latest))
    }

    /// Background check for esphome-device-builder updates. Emits a
    /// notification if a newer version is available. No-op for non-builder
    /// backends.
    pub async fn check_and_notify_device_builder(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
    ) {
        if !backend.is_builder() {
            return;
        }

        let installed = match get_installed_device_builder_version(app_handle) {
            Ok(v) => v,
            Err(e) => {
                debug!("esphome-device-builder version not detected: {}", e);
                return;
            }
        };

        let latest = match self.check_device_builder(backend).await {
            Ok(Some(v)) => v,
            Ok(None) => return,
            Err(e) => {
                warn!("Device-builder update check failed: {}", e);
                return;
            }
        };

        if is_newer_version(&latest, &installed) {
            info!(
                "Device-builder update available: {} -> {} (installed: {})",
                installed, latest, installed
            );

            if let Err(e) = app_handle
                .notification()
                .builder()
                .title("ESPHome Device Builder Update Available")
                .body(format!(
                    "ESPHome Device Builder {} is available (you have {}). \
                     Click 'Check for Updates' in the menu to update.",
                    latest, installed
                ))
                .show()
            {
                error!("Failed to show notification: {}", e);
            }
        } else {
            debug!("ESPHome Device Builder is up to date ({})", installed);
        }
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
        if !backend.is_builder() {
            return None;
        }

        let installed = match get_installed_device_builder_version(app_handle) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "Could not detect installed esphome-device-builder version: {}",
                    e
                );
                return None;
            }
        };

        let latest = match self.check_device_builder(backend).await {
            Ok(Some(v)) => v,
            Ok(None) => return None,
            Err(e) => {
                warn!("Device-builder update check failed: {}", e);
                return None;
            }
        };

        if !is_newer_version(&latest, &installed) {
            info!("ESPHome Device Builder is up to date ({})", installed);
            return None;
        }

        info!(
            "Device-builder update available: {} -> {} (installed: {})",
            installed, latest, installed
        );

        let dialog_app = app_handle.clone();
        let msg = format!(
            "ESPHome Device Builder {} is available.\n\nYou currently have version {}.\n\nWould you like to update now?",
            latest, installed
        );
        let should_update = tokio::task::spawn_blocking(move || {
            dialog_app
                .dialog()
                .message(msg)
                .title("Device Builder Update Available")
                .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                    "Update Now".to_string(),
                    "Later".to_string(),
                ))
                .blocking_show()
        })
        .await
        .unwrap_or(false);

        if should_update {
            Some(latest)
        } else {
            None
        }
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

/// Find the latest beta/pre-release version from PyPI releases.
///
/// Beta versions on PyPI look like "2025.4.0b1", "2025.4.0b2", etc.
/// We find the highest version that contains a beta suffix.
fn find_latest_beta(releases: &HashMap<String, Vec<PyPIRelease>>) -> Option<String> {
    let mut best: Option<String> = None;

    for version_str in releases.keys() {
        // Only consider versions with a beta suffix (e.g. "2025.4.0b1").
        // ESPHome beta releases always use bN naming.
        if !has_beta_suffix(version_str) {
            continue;
        }

        // Skip if not a valid-looking version
        if !version_str
            .chars()
            .next()
            .map_or(false, |c| c.is_ascii_digit())
        {
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

/// Check whether a version string has a beta suffix like "b1", "b2", etc.
/// Matches patterns where a 'b' immediately follows a digit and is followed by
/// one or more digits (e.g. "2025.4.0b1"), which distinguishes it from versions
/// that merely contain the letter 'b' elsewhere.
fn has_beta_suffix(version: &str) -> bool {
    let bytes = version.as_bytes();
    for i in 1..bytes.len().saturating_sub(1) {
        if bytes[i] == b'b'
            && bytes[i - 1].is_ascii_digit()
            && bytes[i + 1].is_ascii_digit()
        {
            return true;
        }
    }
    false
}

/// Find the highest version across all releases on PyPI, including
/// pre-releases. Used for the "beta" device-builder channel where any
/// pre-release counts (a/b/rc/dev), not just `bN` like ESPHome itself.
fn find_latest_any(releases: &HashMap<String, Vec<PyPIRelease>>) -> Option<String> {
    let mut best: Option<String> = None;
    for v in releases.keys() {
        if !v.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            continue;
        }
        match &best {
            None => best = Some(v.clone()),
            Some(curr) => {
                if is_newer_version(v, curr) {
                    best = Some(v.clone());
                }
            }
        }
    }
    best
}

/// Get the installed `esphome-device-builder` package version.
pub fn get_installed_device_builder_version(app_handle: &AppHandle) -> Result<String> {
    let python_path = platform::get_python_path(app_handle)?;

    let mut cmd = std::process::Command::new(&python_path);
    cmd.args([
        "-c",
        "from importlib.metadata import version; print(version('esphome-device-builder'))",
    ]);
    platform::configure_no_window_command(&mut cmd);

    let output = cmd.output().context("Failed to run python")?;

    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if version.is_empty() {
            anyhow::bail!("esphome-device-builder version is empty");
        }
        Ok(version)
    } else {
        anyhow::bail!("esphome-device-builder not installed")
    }
}

/// Get the installed ESPHome version
pub fn get_installed_version(app_handle: &AppHandle) -> Result<String> {
    let python_path = platform::get_python_path(app_handle)?;

    let mut cmd = std::process::Command::new(&python_path);
    cmd.args(["-m", "esphome", "version"]);
    platform::configure_no_window_command(&mut cmd);

    let output = cmd.output().context("Failed to run esphome version")?;

    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // Extract just the version number
        let version = version
            .strip_prefix("Version: ")
            .unwrap_or(&version)
            .to_string();
        Ok(version)
    } else {
        anyhow::bail!("ESPHome not installed")
    }
}

/// A parsed version segment, e.g. "0b1" -> (0, 0, 1)
/// Any pre-release suffix sorts below a stable release (no suffix).
/// ESPHome uses "b" for betas (e.g. "2025.4.0b1") and "-dev" for dev
/// builds (e.g. "2026.5.0-dev").
fn prerelease_ord(tag: &str) -> u8 {
    match tag {
        "b" => 0,
        "dev" => 0,
        _ => 1,
    }
}

/// Parse a version string like "2024.1.0b1" or "2026.5.0-dev" into a
/// comparable representation.
/// Each dot-separated segment becomes (numeric_part, prerelease_order, prerelease_num).
/// A stable segment like "0" becomes (0, 255, 0) so it sorts higher than any pre-release.
fn parse_version(s: &str) -> Vec<(u32, u8, u32)> {
    s.split('.')
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
fn is_newer_version(latest: &str, installed: &str) -> bool {
    let latest_parts = parse_version(latest);
    let installed_parts = parse_version(installed);

    latest_parts > installed_parts
}

#[cfg(test)]
mod tests {
    use super::*;

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
        releases.insert("2025.3.0".to_string(), vec![]);
        releases.insert("2025.4.0b1".to_string(), vec![]);
        releases.insert("2025.4.0b2".to_string(), vec![]);
        releases.insert("2025.3.0b1".to_string(), vec![]);

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, Some("2025.4.0b2".to_string()));
    }

    #[test]
    fn test_find_latest_beta_none() {
        let mut releases = HashMap::new();
        releases.insert("2025.3.0".to_string(), vec![]);
        releases.insert("2025.4.0".to_string(), vec![]);

        let latest = find_latest_beta(&releases);
        assert_eq!(latest, None);
    }
}
