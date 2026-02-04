//! Update checking functionality
//!
//! Queries PyPI for the latest ESPHome version and notifies the user
//! if an update is available.

use anyhow::{Context, Result};
use serde::Deserialize;
use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tracing::{debug, error, info, warn};

use crate::platform;

/// PyPI package info response
#[derive(Debug, Deserialize)]
struct PyPIResponse {
    info: PyPIInfo,
}

#[derive(Debug, Deserialize)]
struct PyPIInfo {
    version: String,
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

    /// Check for updates and return the latest version if available
    pub async fn check(&self) -> Result<Option<String>> {
        debug!("Checking for ESPHome updates");

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
        info!("Latest ESPHome version on PyPI: {}", latest);

        Ok(Some(latest))
    }

    /// Check for updates (user-initiated) - always shows feedback via dialog
    /// Returns Some(version) if user wants to update, None otherwise
    pub async fn check_for_user(&self, app_handle: &AppHandle) -> Option<String> {
        // Get installed version
        let installed = match get_installed_version(app_handle) {
            Ok(v) => v,
            Err(e) => {
                warn!("Could not detect installed version: {}", e);
                app_handle
                    .dialog()
                    .message(format!("Could not detect installed version: {}", e))
                    .kind(MessageDialogKind::Error)
                    .title("Update Check Failed")
                    .blocking_show();
                return None;
            }
        };

        // Check for latest version
        let latest = match self.check().await {
            Ok(Some(v)) => v,
            Ok(None) => {
                app_handle
                    .dialog()
                    .message("Could not determine latest version")
                    .kind(MessageDialogKind::Error)
                    .title("Update Check Failed")
                    .blocking_show();
                return None;
            }
            Err(e) => {
                warn!("Update check failed: {}", e);
                app_handle
                    .dialog()
                    .message(format!("Failed to check for updates: {}", e))
                    .kind(MessageDialogKind::Error)
                    .title("Update Check Failed")
                    .blocking_show();
                return None;
            }
        };

        // Compare versions
        if is_newer_version(&latest, &installed) {
            info!(
                "Update available: {} -> {} (installed: {})",
                installed, latest, installed
            );

            // Ask user if they want to update
            let should_update = app_handle
                .dialog()
                .message(format!(
                    "ESPHome {} is available.\n\nYou currently have version {}.\n\nWould you like to update now?",
                    latest, installed
                ))
                .title("Update Available")
                .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                    "Update Now".to_string(),
                    "Later".to_string(),
                ))
                .blocking_show();

            if should_update {
                return Some(latest);
            }
        } else {
            info!("ESPHome is up to date ({})", installed);

            app_handle
                .dialog()
                .message(format!("ESPHome {} is the latest version.", installed))
                .kind(MessageDialogKind::Info)
                .title("No Updates Available")
                .blocking_show();
        }

        None
    }

    /// Check for updates and notify the user if one is available (background check)
    pub async fn check_and_notify(&self, app_handle: &AppHandle) {
        // Get installed version
        let installed = match get_installed_version(app_handle) {
            Ok(v) => v,
            Err(e) => {
                warn!("Could not detect installed version: {}", e);
                return;
            }
        };

        // Check for latest version
        let latest = match self.check().await {
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

            // Show notification
            if let Err(e) = app_handle
                .notification()
                .builder()
                .title("ESPHome Update Available")
                .body(format!(
                    "ESPHome {} is available (you have {}). Click 'Check for Updates' in the menu to update.",
                    latest, installed
                ))
                .show()
            {
                error!("Failed to show notification: {}", e);
            }
        } else {
            debug!("ESPHome is up to date ({})", installed);
        }
    }

    /// Perform an update to the specified version
    pub async fn update_to(&self, app_handle: &AppHandle, version: &str) -> Result<()> {
        info!("Updating ESPHome to version {}", version);

        let python_path = platform::get_python_path(app_handle)?;

        // Try uv pip install first (faster), fall back to regular pip
        let output = tokio::process::Command::new(&python_path)
            .args([
                "-m",
                "uv",
                "pip",
                "install",
                &format!("esphome=={}", version),
            ])
            .output()
            .await;

        let output = match output {
            Ok(o) if o.status.success() => {
                info!("ESPHome updated successfully to {} (via uv)", version);
                return Ok(());
            }
            _ => {
                // Fall back to regular pip
                debug!("uv not available, falling back to pip");
                tokio::process::Command::new(&python_path)
                    .args(["-m", "pip", "install", &format!("esphome=={}", version)])
                    .output()
                    .await
                    .context("Failed to run pip install")?
            }
        };

        if output.status.success() {
            info!("ESPHome updated successfully to {}", version);
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("pip install failed: {}", stderr)
        }
    }
}

/// Get the installed ESPHome version
fn get_installed_version(app_handle: &AppHandle) -> Result<String> {
    let python_path = platform::get_python_path(app_handle)?;

    let output = std::process::Command::new(&python_path)
        .args(["-m", "esphome", "version"])
        .output()
        .context("Failed to run esphome version")?;

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

/// Compare two version strings and return true if `latest` is newer than `installed`
fn is_newer_version(latest: &str, installed: &str) -> bool {
    let parse_version = |s: &str| -> Vec<u32> {
        s.split('.')
            .filter_map(|part| {
                // Handle versions like "2024.1.0b1" by taking only the numeric part
                part.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .collect()
    };

    let latest_parts = parse_version(latest);
    let installed_parts = parse_version(installed);

    for (l, i) in latest_parts.iter().zip(installed_parts.iter()) {
        match l.cmp(i) {
            std::cmp::Ordering::Greater => return true,
            std::cmp::Ordering::Less => return false,
            std::cmp::Ordering::Equal => continue,
        }
    }

    // If all compared parts are equal, the one with more parts might be newer
    latest_parts.len() > installed_parts.len()
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
        assert!(is_newer_version("2024.1.0", "2024.1.0b1"));
    }
}
