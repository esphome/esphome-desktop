//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tracing::debug;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
use ::windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

/// Get the application data directory
///
/// - macOS: `~/Library/Application Support/io.esphome.builder/`
/// - Windows: `%APPDATA%\io.esphome.builder\`
/// - Linux: `~/.local/share/io.esphome.builder/`
pub fn get_data_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let path = app_handle
        .path()
        .app_data_dir()
        .context("Failed to get app data directory")?;

    // Ensure directory exists
    std::fs::create_dir_all(&path).context("Failed to create data directory")?;

    debug!("Data directory: {:?}", path);
    Ok(path)
}

/// Get the bundled resource directory.
///
/// On Linux we resolve this ourselves so the path is always
/// `<prefix>/lib/esphome-desktop/` — no spaces, no dependence on
/// Tauri's `resource_dir()` which uses the product name ("ESPHome Device Builder").
///
/// The sharun-based AppImage format patches `/usr/…` paths in the binary
/// with random `/tmp/…` tokens, so `std::env::current_exe()` returns a
/// path like `/tmp/.mount_XXX/tmp/<rand>/esphome-desktop` instead of the
/// real `<mount>/bin/esphome-desktop`.  We therefore prefer the `APPDIR`
/// env var that sharun always sets, and fall back to exe-relative
/// resolution for deb/AUR installs where `APPDIR` is absent.
///
/// On macOS and Windows, Tauri's `resource_dir()` works correctly.
fn get_bundled_resource_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // 1. Prefer APPDIR (set by sharun-based AppImage at runtime)
        if let Ok(appdir) = std::env::var("APPDIR") {
            let resource_dir = PathBuf::from(&appdir).join("lib/esphome-desktop");
            if resource_dir.is_dir() {
                debug!("Bundled resource dir (APPDIR): {:?}", resource_dir);
                return Ok(resource_dir);
            }
            debug!(
                "APPDIR set to {:?} but {:?} does not exist",
                appdir, resource_dir
            );
        }

        // 2. Resolve relative to the real executable (deb/AUR installs)
        let exe = std::env::current_exe().context("Failed to get current executable path")?;
        let exe_dir = exe.parent().context("Failed to get executable directory")?;
        // bin/esphome-desktop -> ../lib/esphome-desktop/
        let resource_dir = exe_dir.join("../lib/esphome-desktop");
        if let Ok(resolved) = resource_dir.canonicalize() {
            debug!("Bundled resource dir (resolved): {:?}", resolved);
            return Ok(resolved);
        }

        // 3. Fallback to Tauri's resource_dir for development builds
        let fallback = app_handle
            .path()
            .resource_dir()
            .context("Failed to get resource directory")?;
        debug!("Bundled resource dir (fallback): {:?}", fallback);
        Ok(fallback)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let resource_dir = app_handle
            .path()
            .resource_dir()
            .context("Failed to get resource directory")?;
        debug!("Bundled resource dir: {:?}", resource_dir);
        Ok(resource_dir)
    }
}

/// Get the path to the user Python executable
/// On non-Windows platforms, the bundled Python is copied to user data for updates
pub fn get_python_path(app_handle: &AppHandle) -> Result<PathBuf> {
    let data_dir = get_data_dir(app_handle)?;
    let user_python = data_dir.join("python");

    // Platform-specific Python path
    #[cfg(target_os = "windows")]
    let python_path = user_python.join("python.exe");

    #[cfg(not(target_os = "windows"))]
    let python_path = user_python.join("bin").join("python3");

    if python_path.exists() {
        debug!("Using user Python: {:?}", python_path);
        return Ok(python_path);
    }

    // Fall back to bundled Python (will be copied on first run)
    let resource_dir = get_bundled_resource_dir(app_handle)?;

    #[cfg(target_os = "windows")]
    let bundled_python = resource_dir.join("python").join("python.exe");

    #[cfg(not(target_os = "windows"))]
    let bundled_python = resource_dir.join("python").join("bin").join("python3");

    if bundled_python.exists() {
        debug!("Using bundled Python: {:?}", bundled_python);
        return Ok(bundled_python);
    }

    // Fall back to system Python (for development)
    debug!("Falling back to system Python");
    Ok(PathBuf::from(if cfg!(target_os = "windows") {
        "python"
    } else {
        "python3"
    }))
}

/// Get the Python bin directory (for PATH)
pub fn get_python_bin(app_handle: &AppHandle) -> Result<PathBuf> {
    let data_dir = get_data_dir(app_handle)?;
    let user_python = data_dir.join("python");

    #[cfg(target_os = "windows")]
    let bin_dir = user_python.clone(); // On Windows, python.exe is in the root

    #[cfg(not(target_os = "windows"))]
    let bin_dir = user_python.join("bin");

    // If user Python exists, use it
    if bin_dir.exists() {
        return Ok(bin_dir);
    }

    // Fall back to bundled Python
    let resource_dir = get_bundled_resource_dir(app_handle)?;

    #[cfg(target_os = "windows")]
    let bundled_bin = resource_dir.join("python"); // On Windows, python.exe is in the root

    #[cfg(not(target_os = "windows"))]
    let bundled_bin = resource_dir.join("python").join("bin");

    Ok(bundled_bin)
}

/// Filename of the marker recording which desktop-app version copied the
/// user Python tree. Lives at `<user_python>/.esphome-desktop-version`.
const PYTHON_VERSION_MARKER: &str = ".esphome-desktop-version";

/// Ensure the user Python exists by copying from bundled Python if needed.
///
/// A version marker file is written into the user Python directory after the
/// copy. On subsequent runs, if the marker is missing or doesn't match the
/// current desktop-app version, the directory is wiped and re-copied so that
/// updated app releases ship a fresh Python tree (e.g. new ESPHome version,
/// changed dependencies). Without this, the first-run copy persisted forever.
pub fn ensure_user_python(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let resource_dir = get_bundled_resource_dir(app_handle)?;
        let bundled_python = resource_dir.join("python").join("python.exe");

        if !bundled_python.exists() {
            anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
        }

        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        use tracing::info;

        let data_dir = get_data_dir(app_handle)?;
        let user_python = data_dir.join("python");
        let python_check = user_python.join("bin").join("python3");
        let marker_path = user_python.join(PYTHON_VERSION_MARKER);
        let current_version = env!("CARGO_PKG_VERSION");

        let marker_matches = std::fs::read_to_string(&marker_path)
            .map(|s| s.trim() == current_version)
            .unwrap_or(false);

        let needs_copy = !python_check.exists() || !marker_matches;

        if needs_copy {
            let resource_dir = get_bundled_resource_dir(app_handle)?;
            let bundled_python = resource_dir.join("python");

            if !bundled_python.exists() {
                anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
            }

            // Snapshot the user's pre-existing package versions BEFORE the
            // wipe so we can restore them after the bundled tree is in place.
            // Without this, a user who pip-bumped ESPHome past the bundled
            // version would silently get downgraded by every app self-update.
            let preserved = if python_check.exists() {
                let old_python_bin = python_check.clone();
                PreservedVersions {
                    esphome: read_package_version(&old_python_bin, "esphome"),
                    esphome_device_builder: read_package_version(
                        &old_python_bin,
                        "esphome-device-builder",
                    ),
                }
            } else {
                PreservedVersions::default()
            };

            if user_python.exists() {
                info!(
                    "Removing stale user Python at {:?} (version marker missing or mismatched)",
                    user_python
                );
                std::fs::remove_dir_all(&user_python)
                    .context("Failed to remove stale user Python directory")?;
            }

            info!(
                "Copying bundled Python to user data directory (version {})...",
                current_version
            );

            // Copy the bundled Python to user data
            copy_dir_recursive(&bundled_python, &user_python)?;

            std::fs::write(&marker_path, current_version)
                .context("Failed to write Python version marker")?;

            restore_preserved_versions(&python_check, &preserved);

            info!("User Python ready at {:?}", user_python);
        } else {
            debug!("User Python already up-to-date (version {})", current_version);
        }

        Ok(())
    }
}

/// User-preferred package versions captured before the bundled Python tree
/// is wiped during an app-version refresh. See [`ensure_user_python`].
#[derive(Debug, Default)]
struct PreservedVersions {
    esphome: Option<String>,
    esphome_device_builder: Option<String>,
}

/// Reinstall any preserved package whose pinned version is newer than the
/// version that just shipped in the new bundled Python tree. Bundled wins
/// for ties and for when bundled is newer (so users always benefit from the
/// app's fresher bundle when they haven't explicitly bumped past it). Each
/// reinstall is best-effort — a network failure here logs a warning and
/// falls through to the bundled version rather than blocking app start.
#[cfg(not(target_os = "windows"))]
fn restore_preserved_versions(python_bin: &Path, preserved: &PreservedVersions) {
    use tracing::{info, warn};

    for (package, saved) in [
        ("esphome", preserved.esphome.as_deref()),
        ("esphome-device-builder", preserved.esphome_device_builder.as_deref()),
    ] {
        let Some(saved) = saved else { continue };
        let Some(bundled) = read_package_version(python_bin, package) else {
            // Package isn't in the bundled tree (shouldn't happen for these
            // two, but don't fight it). Skip the restore.
            continue;
        };
        if !crate::update::is_newer_version(saved, &bundled) {
            debug!(
                "Bundled {} {} satisfies user preference {}; not reinstalling",
                package, bundled, saved
            );
            continue;
        }
        info!(
            "Restoring user-preferred {} {} over bundled {}",
            package, saved, bundled
        );
        if let Err(e) = pip_install_blocking(python_bin, package, saved) {
            warn!(
                "Failed to restore {} {}: {}. Continuing with bundled {}.",
                package, saved, e, bundled
            );
        }
    }
}

/// Read the installed version of a Python package via `importlib.metadata`.
/// Returns `None` if the package isn't installed or the call fails.
#[cfg(not(target_os = "windows"))]
fn read_package_version(python_bin: &Path, package: &str) -> Option<String> {
    // Written as a single-line literal with explicit `\n` so each Python
    // statement starts at column zero — avoids any ambiguity about whether
    // a Rust line-continuation strips the source-line indentation.
    let script = format!(
        "from importlib.metadata import version, PackageNotFoundError\ntry: print(version('{}'))\nexcept PackageNotFoundError: pass",
        package
    );
    let mut cmd = std::process::Command::new(python_bin);
    cmd.args(["-c", &script]);
    configure_no_window_command(&mut cmd);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// Hard upper bound on a single `pip install` invocation during the
/// version-restore path. Five minutes is well over the time needed to
/// upgrade `esphome` on a working connection; bounding it prevents a
/// stalled network from hanging app startup indefinitely.
#[cfg(not(target_os = "windows"))]
const PIP_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Maximum length of pip stderr included in a failure error message. pip's
/// resolver and progress output can run to many kilobytes; the actionable
/// failure reason is almost always at the tail, so we truncate to the last
/// N bytes to keep log lines (and downstream UI surfaces) bounded.
#[cfg(not(target_os = "windows"))]
const PIP_STDERR_TAIL_BYTES: usize = 4096;

/// Return `s` trimmed and truncated to the last [`PIP_STDERR_TAIL_BYTES`]
/// bytes, with a marker line if anything was dropped. Backs up to a UTF-8
/// char boundary so the result is always valid `str`.
#[cfg(not(target_os = "windows"))]
fn tail_for_log(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= PIP_STDERR_TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - PIP_STDERR_TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "...(stderr truncated to last {} bytes)\n{}",
        PIP_STDERR_TAIL_BYTES,
        &trimmed[start..]
    )
}

/// Synchronously run `pip install <package>==<version>` with a wall-clock
/// timeout. Pinning the exact version lets pip resolve pre-releases without
/// needing `--pre`. On timeout the child is killed and an error is returned;
/// the caller logs a warning and falls back to the bundled version, so a
/// stalled pip can't block app launch.
#[cfg(not(target_os = "windows"))]
fn pip_install_blocking(python_bin: &Path, package: &str, version: &str) -> Result<()> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let spec = format!("{}=={}", package, version);
    let mut cmd = std::process::Command::new(python_bin);
    cmd.args(["-m", "pip", "install", &spec]);
    cmd.stderr(std::process::Stdio::piped());
    configure_no_window_command(&mut cmd);

    let mut child = cmd.spawn().context("Failed to spawn pip install")?;
    let deadline = Instant::now() + PIP_INSTALL_TIMEOUT;

    // Drain stderr in a background thread. pip's progress bars and resolver
    // diagnostics can easily exceed the OS pipe buffer (~64 KiB on Linux);
    // if nothing reads the parent's end, pip blocks on `write()` mid-install,
    // which would defeat the deadline and hang startup. The reader exits
    // naturally once the child closes its stderr fd (normal exit or kill).
    let mut stderr_thread = child.stderr.take().map(|mut handle| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = handle.read_to_string(&mut buf);
            buf
        })
    });

    loop {
        match child.try_wait().context("Failed to poll pip install")? {
            Some(status) => {
                let stderr = stderr_thread
                    .take()
                    .and_then(|t| t.join().ok())
                    .unwrap_or_default();
                if status.success() {
                    return Ok(());
                }
                anyhow::bail!("pip install {} failed: {}", spec, tail_for_log(&stderr));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = stderr_thread
                        .take()
                        .and_then(|t| t.join().ok())
                        .unwrap_or_default();
                    anyhow::bail!(
                        "pip install {} timed out after {:?}; partial stderr: {}",
                        spec,
                        PIP_INSTALL_TIMEOUT,
                        tail_for_log(&stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Recursively copy a directory
fn copy_dir_recursive(src: &PathBuf, dst: &PathBuf) -> Result<()> {
    use std::fs;

    if !dst.exists() {
        fs::create_dir_all(dst).context("Failed to create destination directory")?;
    }

    for entry in fs::read_dir(src).context("Failed to read source directory")? {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());

        if path.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            fs::copy(&path, &dest_path).context("Failed to copy file")?;
        }
    }

    Ok(())
}

/// Check if ESPHome is available (bundled Python has it pre-installed)
pub fn is_esphome_ready(app_handle: &AppHandle) -> bool {
    let python_path = match get_python_path(app_handle) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // Try to run esphome version
    let mut cmd = std::process::Command::new(&python_path);
    cmd.args(["-m", "esphome", "version"]);
    configure_no_window_command(&mut cmd);

    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Configure std::process::Command to not create a console window on Windows
pub fn configure_no_window_command(cmd: &mut std::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Configure tokio::process::Command to not create a console window on Windows
pub fn configure_no_window_tokio_command(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Configure the daemon child's creation flags on Windows: no console window
/// AND a new process group. The new process group makes the child its own
/// group leader (pgid == pid) so we can later deliver a graceful
/// `CTRL_BREAK_EVENT` to it (and its descendants) for shutdown via
/// `send_ctrl_break`. Sets both flags in one call so neither overwrites the
/// other. No-op on non-Windows (Unix uses `process_group(0)` instead).
pub fn configure_daemon_tokio_command(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags((CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Deliver a graceful `CTRL_BREAK_EVENT` to a child process group on Windows.
///
/// Returns `true` if the event was delivered, `false` if it could not be (the
/// child already exited, or its console is unreachable) — the caller should
/// then fall back to `TerminateProcess`.
///
/// `pid` must be the PID of a child spawned with `CREATE_NEW_PROCESS_GROUP`
/// (see `configure_daemon_tokio_command`); for such a child the process-group
/// id equals its PID. `CTRL_BREAK_EVENT` is the only usable signal here:
/// `CREATE_NEW_PROCESS_GROUP` disables CTRL+C for the group, and unlike
/// `CTRL_C_EVENT` a break can target a specific group id.
///
/// The desktop app is a GUI process with no console, so a bare
/// `GenerateConsoleCtrlEvent` would have nothing to signal through. We
/// transiently attach to the child's (hidden) console, suppress the event in
/// ourselves so we don't self-terminate, broadcast it, then detach. This
/// mutates whole-process console state, so it is serialized under a lock; it
/// is also known to be finicky, hence the caller's `TerminateProcess`
/// fallback.
#[cfg(target_os = "windows")]
pub fn send_ctrl_break(pid: u32) -> bool {
    use ::windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
        CTRL_BREAK_EVENT,
    };

    // Serialize: AttachConsole/FreeConsole/SetConsoleCtrlHandler mutate
    // per-process (not per-thread) console state, so two concurrent sends
    // would corrupt each other.
    static CONSOLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY: serialized Win32 console FFI. We restore the ctrl handler and
    // detach the console before returning regardless of outcome; no handle or
    // console state escapes this function.
    unsafe {
        // Detach from any console we currently hold; otherwise AttachConsole
        // fails with ERROR_ACCESS_DENIED (a process can attach to at most one
        // console). Harmless if we have none.
        let _ = FreeConsole();
        if AttachConsole(pid).is_err() {
            // Child gone, or its console is not reachable.
            return false;
        }
        // Make ourselves ignore the event we are about to broadcast so we
        // don't terminate the desktop along with the child. AttachConsole
        // resets the handler table, so this must come after it.
        let _ = SetConsoleCtrlHandler(None, true);
        let delivered = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid).is_ok();
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = FreeConsole();
        delivered
    }
}

/// Platform-specific initialization
pub fn init() {
    #[cfg(target_os = "macos")]
    macos::init();

    #[cfg(target_os = "windows")]
    windows::init();

    #[cfg(target_os = "linux")]
    linux::init();
}

#[cfg(target_os = "macos")]
mod macos {
    pub fn init() {
        // macOS-specific initialization
        // e.g., set activation policy for menu bar app
    }
}

#[cfg(target_os = "windows")]
mod windows {
    pub fn init() {
        // Windows-specific initialization
    }
}

#[cfg(target_os = "linux")]
mod linux {
    pub fn init() {
        // Linux-specific initialization
    }

    /// Check if a usable appindicator library is available on this system.
    ///
    /// The `tray-icon` crate (via `libappindicator-sys`) will `panic!()` if none of
    /// these shared libraries can be loaded.  We probe for them first so we can
    /// degrade gracefully instead of crashing.
    pub fn is_appindicator_available() -> bool {
        use std::ffi::OsStr;
        for name in &[
            "libayatana-appindicator3.so.1",
            "libappindicator3.so.1",
            "libayatana-appindicator3.so",
            "libappindicator3.so",
        ] {
            if unsafe { libloading::Library::new(OsStr::new(name)) }.is_ok() {
                return true;
            }
        }
        false
    }
}

/// One-shot cleanup of the legacy `/Applications/ESPHome Builder.app` bundle
/// left behind when the desktop app was renamed to "ESPHome Device Builder".
///
/// On the first launch after the rename the user is prompted (via a native
/// dialog) to move the old bundle to the Trash. The decision is recorded as
/// a marker file in the app data directory so the prompt is not repeated.
///
/// User settings and the bundled Python tree live under the bundle
/// identifier (`io.esphome.builder/`), which did not change with the rename,
/// so no data migration is needed.
pub fn cleanup_legacy_macos_app(app_handle: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        use tracing::{info, warn};

        const OLD_APP: &str = "/Applications/ESPHome Builder.app";
        const MARKER_NAME: &str = ".legacy_macos_app_cleanup";

        if !PathBuf::from(OLD_APP).exists() {
            return;
        }

        let data_dir = match get_data_dir(app_handle) {
            Ok(d) => d,
            Err(e) => {
                debug!("Skipping legacy app cleanup; data dir unavailable: {}", e);
                return;
            }
        };

        let marker = data_dir.join(MARKER_NAME);
        if marker.exists() {
            return;
        }

        info!("Legacy {} detected; prompting user to remove it", OLD_APP);

        let dialog_app = app_handle.clone();
        std::thread::spawn(move || {
            let confirmed = dialog_app
                .dialog()
                .message(
                    "An older version of this app named \u{201C}ESPHome Builder\u{201D} \
                     was found in your Applications folder. Move it to the Trash?\n\n\
                     Your settings and configs are not affected.",
                )
                .title("Remove old ESPHome Builder")
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Move to Trash".to_string(),
                    "Keep".to_string(),
                ))
                .blocking_show();

            if confirmed {
                let script = format!(
                    "tell application \"Finder\" to delete POSIX file \"{}\"",
                    OLD_APP
                );
                match std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        info!("Moved {} to Trash", OLD_APP);
                    }
                    Ok(out) => {
                        warn!(
                            "Failed to move {} to Trash: {}",
                            OLD_APP,
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                    }
                    Err(e) => warn!("Failed to spawn osascript: {}", e),
                }
            }

            // Marker is written regardless so the user is not nagged.
            if let Err(e) = std::fs::write(&marker, "") {
                warn!("Failed to write legacy-cleanup marker: {}", e);
            }
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app_handle;
    }
}

/// Returns `true` on Linux when the appindicator library required for system
/// tray support is available, and always `true` on non-Linux platforms (which
/// use native APIs that don't require a separate shared library).
pub fn is_tray_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::is_appindicator_available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}
