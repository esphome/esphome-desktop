//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tracing::debug;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

/// Get the application data directory
///
/// - macOS: `~/Library/Application Support/ESPHome Builder/`
/// - Windows: `%LOCALAPPDATA%\ESPHome Builder\`
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
/// Tauri's `resource_dir()` which uses the product name ("ESPHome Builder").
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

/// Ensure the user Python exists by copying from bundled Python if needed
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

        let needs_copy = !python_check.exists();

        if needs_copy {
            let resource_dir = get_bundled_resource_dir(app_handle)?;
            let bundled_python = resource_dir.join("python");

            if !bundled_python.exists() {
                anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
            }

            info!("Copying bundled Python to user data directory...");

            // Copy the bundled Python to user data
            copy_dir_recursive(&bundled_python, &user_python)?;

            info!("User Python ready at {:?}", user_python);
        } else {
            debug!("User Python already exists");
        }

        Ok(())
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
