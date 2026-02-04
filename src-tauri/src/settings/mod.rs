//! Application settings management
//!
//! Handles loading, saving, and managing user preferences.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::AppHandle;
use tracing::{debug, info};

use crate::platform;

/// Default dashboard port
const DEFAULT_PORT: u16 = 6052;

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Dashboard port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Custom config directory (None = use default)
    #[serde(default)]
    pub config_dir: Option<PathBuf>,

    /// Open dashboard in browser when app starts
    #[serde(default = "default_true")]
    pub open_on_start: bool,

    /// Check for updates automatically
    #[serde(default = "default_true")]
    pub check_updates: bool,

    /// Installed ESPHome version (detected from venv)
    #[serde(skip)]
    pub installed_version: Option<String>,
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            config_dir: None,
            open_on_start: true,
            check_updates: true,
            installed_version: None,
        }
    }
}

impl Settings {
    /// Load settings from disk, or create defaults
    pub fn load(app_handle: &AppHandle) -> Result<Self> {
        let settings_path = Self::settings_path(app_handle)?;

        if settings_path.exists() {
            debug!("Loading settings from {:?}", settings_path);
            let content =
                std::fs::read_to_string(&settings_path).context("Failed to read settings file")?;
            let mut settings: Settings =
                serde_json::from_str(&content).context("Failed to parse settings")?;

            // Detect installed version
            settings.installed_version = detect_installed_version(app_handle).ok();
            Ok(settings)
        } else {
            info!("No settings file found, using defaults");
            let mut settings = Settings::default();
            settings.installed_version = detect_installed_version(app_handle).ok();
            Ok(settings)
        }
    }

    /// Save settings to disk
    pub fn save(&self, app_handle: &AppHandle) -> Result<()> {
        let settings_path = Self::settings_path(app_handle)?;

        // Ensure parent directory exists
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create settings directory")?;
        }

        let content = serde_json::to_string_pretty(self).context("Failed to serialize settings")?;
        std::fs::write(&settings_path, content).context("Failed to write settings file")?;

        info!("Settings saved to {:?}", settings_path);
        Ok(())
    }

    /// Get the path to the settings file
    fn settings_path(app_handle: &AppHandle) -> Result<PathBuf> {
        let data_dir = platform::get_data_dir(app_handle)?;
        Ok(data_dir.join("settings.json"))
    }
}

/// Detect the installed ESPHome version from the venv
fn detect_installed_version(app_handle: &AppHandle) -> Result<String> {
    let python_path = platform::get_python_path(app_handle)?;

    let output = std::process::Command::new(&python_path)
        .args(["-m", "esphome", "version"])
        .output()
        .context("Failed to run esphome version")?;

    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout)
            .trim()
            .to_string();
        // Extract just the version number (e.g., "2024.1.0" from "Version: 2024.1.0")
        let version = version
            .strip_prefix("Version: ")
            .unwrap_or(&version)
            .to_string();
        Ok(version)
    } else {
        anyhow::bail!("ESPHome not installed")
    }
}
