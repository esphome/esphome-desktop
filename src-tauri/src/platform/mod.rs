//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::path::PathBuf;
use tauri::{AppHandle, Manager};
use tracing::debug;

/// Get the application data directory
///
/// - macOS: `~/Library/Application Support/ESPHome Builder/`
/// - Windows: `%APPDATA%\ESPHome Builder\`
/// - Linux: `~/.local/share/esphome-desktop/`
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

/// Get the path to the user Python executable
/// On first run, the bundled Python is copied to user data for updates
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
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .context("Failed to get resource directory")?;

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
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .context("Failed to get resource directory")?;

    #[cfg(target_os = "windows")]
    let bundled_bin = resource_dir.join("python"); // On Windows, python.exe is in the root

    #[cfg(not(target_os = "windows"))]
    let bundled_bin = resource_dir.join("python").join("bin");

    Ok(bundled_bin)
}

/// Ensure the user Python exists by copying from bundled Python if needed
pub fn ensure_user_python(app_handle: &AppHandle) -> Result<()> {
    use tracing::info;

    let data_dir = get_data_dir(app_handle)?;
    let user_python = data_dir.join("python");

    // Check if user Python already exists
    #[cfg(target_os = "windows")]
    let python_check = user_python.join("python.exe");
    #[cfg(not(target_os = "windows"))]
    let python_check = user_python.join("bin").join("python3");

    if python_check.exists() {
        debug!("User Python already exists");
        return Ok(());
    }

    // Get bundled Python path
    let resource_dir = app_handle
        .path()
        .resource_dir()
        .context("Failed to get resource directory")?;
    let bundled_python = resource_dir.join("python");

    if !bundled_python.exists() {
        anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
    }

    info!("Copying bundled Python to user data directory...");

    // Copy the bundled Python to user data
    copy_dir_recursive(&bundled_python, &user_python)?;

    info!("User Python ready at {:?}", user_python);
    Ok(())
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
    std::process::Command::new(&python_path)
        .args(["-m", "esphome", "version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
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
}
