//! Application settings management
//!
//! Handles loading, saving, and managing user preferences.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tracing::{debug, info, warn};

use crate::platform;

/// Default dashboard port
const DEFAULT_PORT: u16 = 6052;

/// ESPHome release channel
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReleaseChannel {
    /// Latest stable release from PyPI
    #[default]
    Stable,
    /// Latest beta/pre-release from PyPI
    Beta,
    /// Latest development build from GitHub (no auto-updates)
    Dev,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// Classic ESPHome dashboard (`esphome dashboard`)
    Classic,
    /// ESPHome device builder, stable release from PyPI
    BuilderStable,
    /// ESPHome device builder, beta/pre-release from PyPI
    #[default]
    BuilderBeta,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Classic => write!(f, "Classic ESPHome Dashboard"),
            Self::BuilderStable => write!(f, "ESPHome Device Builder (stable)"),
            Self::BuilderBeta => write!(f, "ESPHome Device Builder (beta)"),
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
    /// Load settings from disk, or create defaults.
    ///
    /// A settings file that exists but cannot be read or parsed must NOT abort
    /// startup: `AppState::new` propagates this error and the Tauri `setup`
    /// hook turns it into a failed launch, so a single corrupt byte in
    /// settings.json would brick the whole app. Instead `load_settings_file`
    /// recovers to defaults (and moves the bad file aside) on corruption. The
    /// only error this returns now is a genuine inability to resolve the data
    /// directory, which is an environment failure worth surfacing.
    pub fn load(app_handle: &AppHandle) -> Result<Self> {
        let settings_path = Self::settings_path(app_handle)?;

        let mut settings = load_settings_file(&settings_path);
        settings.installed_version = detect_installed_version(app_handle).ok();
        Ok(settings)
    }

    /// Save settings to disk
    pub fn save(&self, app_handle: &AppHandle) -> Result<()> {
        let settings_path = Self::settings_path(app_handle)?;

        // Ensure parent directory exists
        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent).context("Failed to create settings directory")?;
        }

        let content = serde_json::to_string_pretty(self).context("Failed to serialize settings")?;
        // Atomic write: a torn `fs::write` would leave settings.json truncated,
        // failing the next parse and silently resetting every user preference.
        crate::util::atomic_write(&settings_path, content)
            .context("Failed to write settings file")?;

        info!("Settings saved to {:?}", settings_path);
        Ok(())
    }

    /// Get the path to the settings file
    fn settings_path(app_handle: &AppHandle) -> Result<PathBuf> {
        let data_dir = platform::get_data_dir(app_handle)?;
        Ok(data_dir.join("settings.json"))
    }
}

/// Read and parse the settings file at `path`, recovering gracefully from a
/// missing or corrupt file.
///
/// - File absent → defaults (the first-run case).
/// - File present and valid → the parsed settings.
/// - File present but unreadable or unparseable (truncated by a torn write
///   from a pre-atomic-write build, a bad hand-edit, or on-disk corruption) →
///   the bad file is moved aside to `<name>.corrupt` and defaults are
///   returned. This deliberately favors a working app with reset preferences
///   over a non-starting one.
///
/// Does not populate `installed_version`; that requires an `AppHandle` and is
/// filled in by the caller.
fn load_settings_file(path: &Path) -> Settings {
    if !path.exists() {
        info!("No settings file found, using defaults");
        return Settings::default();
    }

    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            warn!(
                "Failed to read settings file {:?} ({}); using defaults",
                path, e
            );
            return Settings::default();
        }
    };

    match serde_json::from_str::<Settings>(&content) {
        Ok(settings) => {
            debug!("Loaded settings from {:?}", path);
            settings
        }
        Err(e) => {
            warn!(
                "Settings file {:?} is corrupt ({}); backing it up and using defaults",
                path, e
            );
            back_up_corrupt_settings(path);
            Settings::default()
        }
    }
}

/// Move a corrupt settings file aside to `<name>.corrupt` so it isn't silently
/// overwritten by the next save, preserving it for recovery/debugging.
///
/// Best-effort: if the rename fails we just log and proceed with defaults, and
/// the bad file will be overwritten on the next successful save.
fn back_up_corrupt_settings(path: &Path) {
    let backup = path.with_extension("json.corrupt");
    match std::fs::rename(path, &backup) {
        Ok(()) => warn!("Moved corrupt settings file to {:?}", backup),
        Err(e) => warn!("Failed to back up corrupt settings file {:?}: {}", path, e),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Create a unique, empty temp directory for a test and return its path.
    /// The process id plus a per-test tag keeps both intra-process parallelism
    /// and two concurrent `cargo test` binaries on the same host from
    /// colliding.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("esphome_settings_{}_{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn missing_file_yields_defaults() {
        let dir = unique_temp_dir("missing");
        let path = dir.join("settings.json");

        let settings = load_settings_file(&path);

        assert_eq!(settings.port, DEFAULT_PORT);
        assert_eq!(settings.backend, Backend::default());
        assert!(!path.exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn valid_file_round_trips() {
        let dir = unique_temp_dir("valid");
        let path = dir.join("settings.json");

        let original = Settings {
            port: 1234,
            open_on_start: false,
            release_channel: ReleaseChannel::Beta,
            backend: Backend::Classic,
            ..Default::default()
        };
        let content = serde_json::to_string_pretty(&original).expect("serialize");
        fs::write(&path, content).expect("write settings");

        let loaded = load_settings_file(&path);

        assert_eq!(loaded.port, 1234);
        assert!(!loaded.open_on_start);
        assert_eq!(loaded.release_channel, ReleaseChannel::Beta);
        assert_eq!(loaded.backend, Backend::Classic);
        // A successful parse must not move the file aside.
        assert!(path.exists());
        assert!(!path.with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_recovers_to_defaults_and_is_backed_up() {
        let dir = unique_temp_dir("corrupt");
        let path = dir.join("settings.json");

        // Malformed JSON, like a torn write from a pre-atomic-write build.
        let garbage = "{ \"port\": 1234, \"backend\": ";
        fs::write(&path, garbage).expect("write garbage");

        let settings = load_settings_file(&path);

        // Recovered to defaults rather than erroring out.
        assert_eq!(settings.port, DEFAULT_PORT);
        // Original moved aside, content preserved for recovery.
        let backup = path.with_extension("json.corrupt");
        assert!(!path.exists(), "corrupt file should have been moved");
        assert!(backup.exists(), "corrupt file should be backed up");
        assert_eq!(fs::read_to_string(&backup).expect("read backup"), garbage);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_file_recovers_to_defaults() {
        // An empty settings.json is the exact symptom of a torn write: the
        // file exists but parses to an EOF error. It must recover, not brick.
        let dir = unique_temp_dir("empty");
        let path = dir.join("settings.json");
        fs::write(&path, "").expect("write empty");

        let settings = load_settings_file(&path);

        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(path.with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_json_object_uses_serde_defaults() {
        // `{}` is valid JSON and every field has a serde default, so this is a
        // legitimate (not corrupt) file — it must parse, not be backed up.
        let dir = unique_temp_dir("empty_obj");
        let path = dir.join("settings.json");
        fs::write(&path, "{}").expect("write empty object");

        let settings = load_settings_file(&path);

        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(settings.open_on_start);
        assert!(path.exists());
        assert!(!path.with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
