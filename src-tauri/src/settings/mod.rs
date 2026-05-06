//! Application settings management
//!
//! Handles loading, saving, and managing user preferences.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use tauri::AppHandle;
use tracing::{debug, info};

use crate::platform;

/// Default dashboard port
const DEFAULT_PORT: u16 = 6052;

/// ESPHome release channel
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseChannel {
    /// Latest stable release from PyPI
    Stable,
    /// Latest beta/pre-release from PyPI
    Beta,
    /// Latest development build from GitHub (no auto-updates)
    Dev,
}

impl Default for ReleaseChannel {
    fn default() -> Self {
        Self::Stable
    }
}

impl fmt::Display for ReleaseChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stable => write!(f, "Stable"),
            Self::Beta => write!(f, "Beta"),
            Self::Dev => write!(f, "Dev"),
        }
    }
}

/// Which backend the daemon should run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Classic ESPHome dashboard (`esphome dashboard`)
    Classic,
    /// ESPHome device builder, stable release from PyPI
    BuilderStable,
    /// ESPHome device builder, beta/pre-release from PyPI
    BuilderBeta,
}

impl Default for Backend {
    fn default() -> Self {
        Self::Classic
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Classic => write!(f, "Classic ESPHome Dashboard"),
            Self::BuilderStable => write!(f, "ESPHome Builder (stable)"),
            Self::BuilderBeta => write!(f, "ESPHome Builder (beta)"),
        }
    }
}

impl Backend {
    /// True for any of the device-builder variants.
    pub fn is_builder(self) -> bool {
        matches!(self, Self::BuilderStable | Self::BuilderBeta)
    }
}

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

    /// Release channel (stable, beta, or dev)
    #[serde(default)]
    pub release_channel: ReleaseChannel,

    /// Active backend (classic dashboard or device builder variant)
    #[serde(default)]
    pub backend: Backend,

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
            release_channel: ReleaseChannel::default(),
            backend: Backend::default(),
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

    let mut cmd = std::process::Command::new(&python_path);
    cmd.args(["-m", "esphome", "version"]);
    platform::configure_no_window_command(&mut cmd);

    let output = cmd.output().context("Failed to run esphome version")?;

    if output.status.success() {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
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
