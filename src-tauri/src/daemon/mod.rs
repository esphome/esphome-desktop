//! ESPHome daemon process management
//!
//! Handles starting, stopping, and monitoring the ESPHome dashboard process.

use anyhow::{Context, Result};
use std::fs::File;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::AppHandle;
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

use crate::platform;
use crate::settings::Settings;

fn ensure_runtime_esphome_wrapper(python_path: &PathBuf, python_bin_dir: &PathBuf) -> Result<()> {
    use std::io::Write;

    #[cfg(target_os = "windows")]
    {
        let wrapper_path = python_bin_dir.join("esphome.bat");
        let mut file = std::fs::File::create(&wrapper_path)
            .with_context(|| format!("Failed to create {:?}", wrapper_path))?;
        file.write_all(
            format!("@echo off\r\n\"{}\" -m esphome %*\r\n", python_path.display()).as_bytes(),
        )
        .with_context(|| format!("Failed to write {:?}", wrapper_path))?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::os::unix::fs::PermissionsExt;

        let wrapper_path = python_bin_dir.join("esphome");
        let mut file = std::fs::File::create(&wrapper_path)
            .with_context(|| format!("Failed to create {:?}", wrapper_path))?;
        file.write_all(
            format!("#!/bin/sh\nexec \"{}\" -m esphome \"$@\"\n", python_path.display()).as_bytes(),
        )
        .with_context(|| format!("Failed to write {:?}", wrapper_path))?;

        let mut perms = std::fs::metadata(&wrapper_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&wrapper_path, perms)?;
    }

    Ok(())
}

/// Manages the ESPHome dashboard process
pub struct DaemonManager {
    /// The running process, if any
    process: Arc<Mutex<Option<Child>>>,
    /// Path to bundled Python executable
    python_path: PathBuf,
    /// Path to bundled Python bin directory (for PATH)
    python_bin_dir: PathBuf,
    /// Path to config directory
    config_dir: PathBuf,
    /// Path to logs directory
    logs_dir: PathBuf,
    /// Dashboard port
    port: u16,
    /// Whether the daemon is running
    running: Arc<AtomicBool>,
}

impl DaemonManager {
    /// Create a new daemon manager
    pub fn new(app_handle: &AppHandle, settings: &Settings) -> Result<Self> {
        let data_dir = platform::get_data_dir(app_handle)?;
        let python_path = platform::get_python_path(app_handle)?;
        let python_bin_dir = platform::get_python_bin(app_handle)?;

        // Use ~/esphome as the default config directory
        let config_dir = settings.config_dir.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .map(|h| h.join("esphome"))
                .unwrap_or_else(|| PathBuf::from("esphome"))
        });
        std::fs::create_dir_all(&config_dir).context("Failed to create config directory")?;

        // Create logs directory in app data
        let logs_dir = data_dir.join("logs");
        std::fs::create_dir_all(&logs_dir).context("Failed to create logs directory")?;

        Ok(Self {
            process: Arc::new(Mutex::new(None)),
            python_path,
            python_bin_dir,
            config_dir,
            logs_dir,
            port: settings.port,
            running: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Start the ESPHome dashboard
    pub async fn start(&self) -> Result<()> {
        if self.running.load(Ordering::SeqCst) {
            info!("Daemon already running");
            return Ok(());
        }

        info!("Starting ESPHome dashboard on port {}", self.port);
        debug!("Python path: {:?}", self.python_path);
        debug!("Python bin: {:?}", self.python_bin_dir);
        debug!("Config dir: {:?}", self.config_dir);
        debug!("Logs dir: {:?}", self.logs_dir);

        // Verify Python exists
        if !self.python_path.exists() {
            anyhow::bail!("Python not found at {:?}", self.python_path);
        }

        // Ensure `esphome` resolves to a direct Python module wrapper instead of
        // any packaged launcher shim (e.g. uv trampoline).
        ensure_runtime_esphome_wrapper(&self.python_path, &self.python_bin_dir)?;

        // Open log file for stdout and stderr combined
        let log_path = self.logs_dir.join("dashboard.log");
        let log_file = File::create(&log_path).context("Failed to create log file")?;
        let log_file_clone = log_file.try_clone().context("Failed to clone log file handle")?;

        info!("Dashboard logs: {:?}", log_path);

        // Build the command
        let mut cmd = Command::new(&self.python_path);
        cmd.args([
            "-m",
            "esphome",
            "dashboard",
            self.config_dir.to_str().unwrap_or("."),
            "--address",
            "127.0.0.1",
            "--port",
            &self.port.to_string(),
        ])
        // Set working directory to config dir (required for PlatformIO)
        .current_dir(&self.config_dir)
        // Redirect stdout/stderr to single log file
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_clone))
        .kill_on_drop(true);

        // Create new process group on Unix so we can kill all children
        #[cfg(unix)]
        cmd.process_group(0);

        // Prevent a console window from staying open on Windows
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW.0);

        // Set environment variables
        cmd.env("ESPHOME_DASHBOARD", "1");

        // Add Python bin directory and Scripts to PATH for subprocess needs
        let current_path = std::env::var("PATH").unwrap_or_default();

        // Use platform-specific PATH separator (';' on Windows, ':' on Unix)
        #[cfg(target_os = "windows")]
        let path_sep = ";";
        #[cfg(not(target_os = "windows"))]
        let path_sep = ":";

        // On Windows, also add Scripts directory so esphome.exe (pip launcher) can be found
        #[cfg(target_os = "windows")]
        let new_path = {
            let scripts_dir = self.python_bin_dir.join("Scripts");
            format!("{}{}{}{}{}", scripts_dir.display(), path_sep, self.python_bin_dir.display(), path_sep, current_path)
        };

        #[cfg(not(target_os = "windows"))]
        let new_path = format!("{}{}{}", self.python_bin_dir.display(), path_sep, current_path);
        info!("PATH set to: {}", new_path);
        cmd.env("PATH", new_path);

        let child = cmd.spawn().context("Failed to spawn ESPHome process")?;

        let mut process = self.process.lock().await;
        *process = Some(child);
        self.running.store(true, Ordering::SeqCst);

        // Start health check task
        let running = self.running.clone();
        let port = self.port;
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                if !running.load(Ordering::SeqCst) {
                    break;
                }
                match health_check(port).await {
                    Ok(true) => debug!("Health check passed"),
                    Ok(false) => warn!("Health check failed - dashboard may be starting"),
                    Err(e) => warn!("Health check error: {}", e),
                }
            }
        });

        info!("ESPHome dashboard started");
        Ok(())
    }

    /// Stop the ESPHome dashboard
    pub async fn stop(&self) -> Result<()> {
        if !self.running.load(Ordering::SeqCst) {
            info!("Daemon not running");
            return Ok(());
        }

        info!("Stopping ESPHome dashboard");
        self.running.store(false, Ordering::SeqCst);

        let mut process = self.process.lock().await;
        if let Some(mut child) = process.take() {
            // Try graceful shutdown first - kill the process group on Unix
            #[cfg(unix)]
            {
                use nix::sys::signal::{killpg, Signal};
                use nix::unistd::Pid;

                if let Some(pid) = child.id() {
                    // Send SIGTERM to the process group to kill all children
                    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }
            }

            // Wait for process to exit (with timeout)
            let timeout = tokio::time::timeout(tokio::time::Duration::from_secs(5), child.wait());

            match timeout.await {
                Ok(Ok(status)) => info!("ESPHome dashboard exited with status: {}", status),
                Ok(Err(e)) => warn!("Error waiting for process: {}", e),
                Err(_) => {
                    warn!("Timeout waiting for graceful shutdown, forcing kill");
                    #[cfg(unix)]
                    {
                        use nix::sys::signal::{killpg, Signal};
                        use nix::unistd::Pid;
                        if let Some(pid) = child.id() {
                            // Force kill the process group
                            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
                        }
                    }
                    let _ = child.kill().await;
                }
            }
        }

        info!("ESPHome dashboard stopped");
        Ok(())
    }

    /// Restart the daemon
    pub async fn restart(&self) -> Result<()> {
        self.stop().await?;
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        self.start().await
    }

    /// Check if the daemon is running
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Get the port the daemon is running on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the config directory
    pub fn config_dir(&self) -> &PathBuf {
        &self.config_dir
    }

    /// Get the logs directory
    pub fn logs_dir(&self) -> &PathBuf {
        &self.logs_dir
    }
}

/// Perform a health check on the dashboard
async fn health_check(port: u16) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?;

    let url = format!("http://localhost:{}/", port);
    match client.get(&url).send().await {
        Ok(response) => Ok(response.status().is_success()),
        Err(_) => Ok(false),
    }
}
