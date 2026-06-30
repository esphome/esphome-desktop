//! Application settings management
//!
//! Handles loading, saving, and managing user preferences.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::ErrorKind;
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

/// Which device-builder channel the daemon should run. The classic ESPHome
/// dashboard backend was removed in line with ESPHome 2026.6.0 retiring the
/// legacy in-tree dashboard; the daemon now always launches the device builder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    /// ESPHome device builder, stable release from PyPI
    BuilderStable,
    /// ESPHome device builder, beta/pre-release from PyPI
    #[default]
    BuilderBeta,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BuilderStable => write!(f, "ESPHome Device Builder (stable)"),
            Self::BuilderBeta => write!(f, "ESPHome Device Builder (beta)"),
        }
    }
}

/// Deserialize the backend, tolerating legacy or unknown values by falling back
/// to the default. An old settings file selecting the removed classic dashboard
/// (`"backend": "classic"`) must migrate to the default device builder rather
/// than failing the whole parse, which would discard every other preference via
/// the corrupt-file recovery path.
fn deserialize_backend<'de, D>(deserializer: D) -> Result<Backend, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Deserialize into a generic value so a non-string `backend` (null, number,
    // bool from a hand-edited or future file) falls back to the default too,
    // rather than failing the whole parse and discarding every other preference.
    let raw = serde_json::Value::deserialize(deserializer)?;
    Ok(match raw.as_str() {
        Some("builder_stable") => Backend::BuilderStable,
        Some("builder_beta") => Backend::BuilderBeta,
        _ => Backend::default(),
    })
}

/// Deserialize the dashboard port, falling back to the default for a zero,
/// out-of-range, or non-numeric value.
///
/// Port `0` is the dangerous case: a server reads it as "pick any free
/// ephemeral port," but this app uses the configured value verbatim for the
/// health check (`health_check_url`) and the dashboard URL it opens, never the
/// port the backend actually bound. A persisted `{"port": 0}` (hand-edited
/// file) would therefore leave the dashboard permanently unreachable with no
/// visible error. A non-number (null, string, bool from a hand-edited or future
/// file) likewise falls back here rather than failing the whole parse and
/// discarding every other preference via the corrupt-file recovery path.
fn deserialize_port<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = serde_json::Value::deserialize(deserializer)?;
    let port = raw.as_u64().and_then(|n| u16::try_from(n).ok()).unwrap_or(0);
    Ok(if port == 0 { DEFAULT_PORT } else { port })
}

/// Returns true if the persisted settings file selects the removed classic
/// dashboard backend. Used at startup to force a fresh bundled device builder
/// for users migrating off classic. Tolerant of a missing or unreadable file.
pub fn persisted_backend_was_classic(app_handle: &AppHandle) -> bool {
    let Ok(path) = Settings::settings_path(app_handle) else {
        return false;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|v| {
            v.get("backend")
                .and_then(|b| b.as_str())
                .map(|b| b == "classic")
        })
        .unwrap_or(false)
}

/// Application settings
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Dashboard port
    #[serde(default = "default_port", deserialize_with = "deserialize_port")]
    pub port: u16,

    /// Custom config directory (None = use default)
    #[serde(default)]
    pub config_dir: Option<PathBuf>,

    /// Open dashboard in browser when app starts
    #[serde(default = "default_true")]
    pub open_on_start: bool,

    /// Launch the app automatically at system login/boot. On by default so a
    /// remote builder comes back online after a reboot; the OS login item is
    /// reconciled to this value on every launch.
    #[serde(default = "default_true")]
    pub launch_at_startup: bool,

    /// Check for updates automatically
    #[serde(default = "default_true")]
    pub check_updates: bool,

    /// Release channel (stable, beta, or dev)
    #[serde(default)]
    pub release_channel: ReleaseChannel,

    /// Active device-builder channel (stable or beta)
    #[serde(default, deserialize_with = "deserialize_backend")]
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
            launch_at_startup: true,
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
    // Branch on the read result rather than pre-checking `exists()`:
    // `Path::exists()` returns `false` for any stat failure (e.g. a permission
    // error), which would misclassify an unreadable file as first-run and skip
    // the backup path. `NotFound` is the only genuine first-run case; every
    // other read error is treated as a corrupt/unreadable file.
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            info!("No settings file found, using defaults");
            return Settings::default();
        }
        Err(e) => {
            warn!(
                "Settings file {:?} is unreadable ({}); backing it up and using defaults",
                path, e
            );
            back_up_corrupt_settings(path);
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
///
/// If `<name>.corrupt` already exists from a prior recovery, a numbered suffix
/// (`<name>.corrupt.1`, `.2`, ...) is chosen so an earlier backup is not lost
/// (Unix `rename` overwrites) or the rename made to fail (Windows `rename`
/// errors on an existing destination).
fn back_up_corrupt_settings(path: &Path) {
    let backup = unique_backup_path(path);
    match std::fs::rename(path, &backup) {
        Ok(()) => warn!("Moved corrupt settings file to {:?}", backup),
        Err(e) => warn!("Failed to back up corrupt settings file {:?}: {}", path, e),
    }
}

/// Pick a backup path that does not already exist, starting at `<name>.corrupt`
/// and falling back to `<name>.corrupt.N` for the first free `N`.
fn unique_backup_path(path: &Path) -> PathBuf {
    let base = path.with_extension("json.corrupt");
    if !base.exists() {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = base.with_extension(format!("corrupt.{n}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
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
            backend: Backend::BuilderStable,
            ..Default::default()
        };
        let content = serde_json::to_string_pretty(&original).expect("serialize");
        fs::write(&path, content).expect("write settings");

        let loaded = load_settings_file(&path);

        assert_eq!(loaded.port, 1234);
        assert!(!loaded.open_on_start);
        assert_eq!(loaded.release_channel, ReleaseChannel::Beta);
        assert_eq!(loaded.backend, Backend::BuilderStable);
        // A successful parse must not move the file aside.
        assert!(path.exists());
        assert!(!path.with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_classic_backend_migrates_to_default() {
        // An old settings file selecting the removed classic dashboard backend
        // must migrate to the default device builder (beta), keep its other
        // fields, and NOT be treated as corrupt (which would discard every
        // other preference).
        let dir = unique_temp_dir("classic");
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"port":1234,"backend":"classic"}"#).expect("write settings");

        let settings = load_settings_file(&path);

        assert_eq!(settings.backend, Backend::BuilderBeta);
        assert_eq!(settings.port, 1234);
        assert!(path.exists());
        assert!(!path.with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn zero_port_falls_back_to_default() {
        // A persisted port of 0 would leave the dashboard unreachable: the app
        // uses the configured value for the health check and dashboard URL, not
        // the ephemeral port the backend would actually bind. Normalize it to
        // the default while keeping every other preference.
        let dir = unique_temp_dir("zero_port");
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"port":0,"backend":"builder_stable"}"#).expect("write settings");

        let settings = load_settings_file(&path);

        assert_eq!(settings.port, DEFAULT_PORT);
        assert_eq!(settings.backend, Backend::BuilderStable);
        assert!(!path.with_extension("json.corrupt").exists());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn out_of_range_or_non_numeric_port_falls_back_to_default() {
        // A port above u16::MAX or a non-numeric value must fall back to the
        // default instead of failing the whole parse and discarding every other
        // preference via corrupt-file recovery.
        for body in [r#"{"port":70000}"#, r#"{"port":"6052"}"#, r#"{"port":null}"#] {
            let dir = unique_temp_dir("bad_port");
            let path = dir.join("settings.json");
            fs::write(&path, body).expect("write settings");

            let settings = load_settings_file(&path);

            assert_eq!(settings.port, DEFAULT_PORT, "body: {body}");
            assert!(!path.with_extension("json.corrupt").exists(), "body: {body}");

            let _ = fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn non_string_backend_value_falls_back_to_default() {
        // A malformed (non-string) backend value must fall back to the default
        // instead of failing the whole parse, which would discard every other
        // preference via corrupt-file recovery.
        let dir = unique_temp_dir("bad_backend");
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"port":1234,"backend":null}"#).expect("write settings");

        let settings = load_settings_file(&path);

        assert_eq!(settings.backend, Backend::BuilderBeta);
        assert_eq!(settings.port, 1234);
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

    #[test]
    fn second_corruption_uses_numbered_backup() {
        // A prior recovery already left a `settings.json.corrupt`. A second
        // corruption must not clobber it (Unix `rename`) or fail to back up
        // (Windows `rename`): the new backup lands at `.corrupt.1` and the
        // original `.corrupt` is preserved.
        let dir = unique_temp_dir("second_corrupt");
        let path = dir.join("settings.json");

        let first_backup = path.with_extension("json.corrupt");
        let old_garbage = "first corruption";
        fs::write(&first_backup, old_garbage).expect("write prior backup");

        let new_garbage = "{ \"port\": 1234, \"backend\": ";
        fs::write(&path, new_garbage).expect("write garbage");

        let settings = load_settings_file(&path);

        assert_eq!(settings.port, DEFAULT_PORT);
        assert!(!path.exists(), "corrupt file should have been moved");
        // Prior backup untouched.
        assert_eq!(
            fs::read_to_string(&first_backup).expect("read prior backup"),
            old_garbage
        );
        // New backup lands at the numbered fallback with content preserved.
        let numbered = first_backup.with_extension("corrupt.1");
        assert!(numbered.exists(), "second backup should be numbered");
        assert_eq!(
            fs::read_to_string(&numbered).expect("read numbered backup"),
            new_garbage
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
