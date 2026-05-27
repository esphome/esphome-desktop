//! Desktop application self-updater.
//!
//! Wraps `tauri-plugin-updater` to check GitHub Releases for a new version
//! of the ESPHome Device Builder desktop app itself (not the bundled ESPHome
//! Python package — that lives in [`crate::update`]).
//!
//! The app self-update ships with a fresh Python bundle (ESPHome and
//! `esphome-device-builder` pre-installed at build time). Installing it
//! overwrites the user's `python/` directory, wiping any pip-installed
//! version bumps. The check helpers here return [`NextStep`] so callers
//! that orchestrate the full app → ESPHome → device-builder sequence can
//! skip the Python-package checks when the app itself is about to roll.

use std::time::Duration;

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;
use tracing::{debug, error, info, warn};

/// Whether the orchestrator should proceed to check the Python packages
/// (`esphome` / `esphome-device-builder`) after the desktop self-update
/// check completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextStep {
    /// Carry on with ESPHome / device-builder checks.
    Continue,
    /// Stop — an app update is pending or actively installing, and pip-installing
    /// a Python-package update right now would just get overwritten by the new
    /// bundled Python on the next launch.
    Skip,
}

/// User-initiated app-update check. Always shows the "update available" dialog;
/// the "you're up to date" dialog is only shown when `show_no_update_dialog`
/// is true, so chained callers ("Check for Updates") can stay quiet and fall
/// through to the ESPHome check instead. Errors are always surfaced.
pub async fn check_for_user(app_handle: &AppHandle, show_no_update_dialog: bool) -> NextStep {
    let updater = match app_handle.updater() {
        Ok(u) => u,
        Err(e) => {
            warn!("Updater not available: {}", e);
            show_error(app_handle, format!("Updater not available: {}", e)).await;
            return NextStep::Continue;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            info!(
                "Desktop update available: {} (current: {})",
                update.version, update.current_version
            );

            let new_version = update.version.clone();
            let current_version = update.current_version.clone();
            let notes = update.body.clone().unwrap_or_default();

            let dialog_app = app_handle.clone();
            let msg = format_update_prompt(&current_version, &new_version, &notes);
            let confirmed = tokio::task::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message(msg)
                    .title("Desktop Update Available")
                    .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                        "Update Now".to_string(),
                        "Later".to_string(),
                    ))
                    .blocking_show()
            })
            .await
            .unwrap_or(false);

            if !confirmed {
                // User saw the dialog and declined — fall through to ESPHome check.
                return NextStep::Continue;
            }

            apply_update(app_handle, update).await;
            // The install completed (or failed and surfaced an error). Either
            // way, do NOT proceed to ESPHome — on success the new bundled Python
            // will replace ours; on failure the user is in a state we shouldn't
            // compound with more pip activity.
            NextStep::Skip
        }
        Ok(None) => {
            let current = app_handle.package_info().version.to_string();
            info!("Desktop app is up to date ({})", current);
            if show_no_update_dialog {
                let dialog_app = app_handle.clone();
                let msg = format!("ESPHome Device Builder {} is the latest version.", current);
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
            NextStep::Continue
        }
        Err(e) => {
            warn!("Desktop update check failed: {}", e);
            show_error(app_handle, format!("Failed to check for updates: {}", e)).await;
            NextStep::Continue
        }
    }
}

/// Background check. Only surfaces a notification when a new version is
/// available; stays silent for "no update" and for errors. Returns
/// [`NextStep::Skip`] when an update is available so the background loop
/// can skip the Python-package checks until the user installs.
pub async fn check_and_notify(app_handle: &AppHandle) -> NextStep {
    let updater = match app_handle.updater() {
        Ok(u) => u,
        Err(e) => {
            debug!("Updater not available for background check: {}", e);
            return NextStep::Continue;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            info!(
                "Desktop update available in background: {} (current: {})",
                update.version, update.current_version
            );
            if let Err(e) = app_handle
                .notification()
                .builder()
                .title("ESPHome Device Builder Update Available")
                .body(format!(
                    "Version {} is available (you have {}). Open the tray menu and choose \"Check for Updates...\" to install.",
                    update.version, update.current_version
                ))
                .show()
            {
                error!("Failed to show desktop-update notification: {}", e);
            }
            NextStep::Skip
        }
        Ok(None) => {
            debug!("Desktop app is up to date (background check)");
            NextStep::Continue
        }
        Err(e) => {
            debug!("Background desktop update check failed: {}", e);
            NextStep::Continue
        }
    }
}

/// Download and install the given update, then prompt the user to restart.
async fn apply_update(app_handle: &AppHandle, update: tauri_plugin_updater::Update) {
    let new_version = update.version.clone();

    let mut downloaded: u64 = 0;
    let mut last_logged = std::time::Instant::now();
    let result = update
        .download_and_install(
            move |chunk, total| {
                downloaded = downloaded.saturating_add(chunk as u64);
                // Throttle progress logs to once per second.
                if last_logged.elapsed() >= Duration::from_secs(1) {
                    if let Some(total) = total {
                        info!(
                            "Downloading desktop update: {}/{} bytes",
                            downloaded, total
                        );
                    } else {
                        info!("Downloading desktop update: {} bytes", downloaded);
                    }
                    last_logged = std::time::Instant::now();
                }
            },
            || info!("Desktop update download complete; installing…"),
        )
        .await;

    match result {
        Ok(()) => {
            info!("Desktop update {} installed", new_version);
            let dialog_app = app_handle.clone();
            let msg = format!(
                "ESPHome Device Builder {} has been installed.\n\nRestart now to use the new version?",
                new_version
            );
            let restart = tokio::task::spawn_blocking(move || {
                dialog_app
                    .dialog()
                    .message(msg)
                    .title("Update Installed")
                    .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                        "Restart Now".to_string(),
                        "Later".to_string(),
                    ))
                    .blocking_show()
            })
            .await
            .unwrap_or(false);

            if restart {
                info!("Restarting to apply desktop update");
                app_handle.restart();
            }
        }
        Err(e) => {
            error!("Desktop update install failed: {}", e);
            show_error(app_handle, format!("Failed to install update: {}", e)).await;
        }
    }
}

async fn show_error(app_handle: &AppHandle, msg: String) {
    let dialog_app = app_handle.clone();
    let _ = tokio::task::spawn_blocking(move || {
        dialog_app
            .dialog()
            .message(msg)
            .kind(MessageDialogKind::Error)
            .title("Update Check Failed")
            .blocking_show();
    })
    .await;
}

fn format_update_prompt(current: &str, new: &str, notes: &str) -> String {
    let trimmed_notes = notes.trim();
    if trimmed_notes.is_empty() {
        format!(
            "ESPHome Device Builder {} is available.\n\nYou currently have version {}.\n\nWould you like to download and install it now?",
            new, current
        )
    } else {
        // Keep release notes short in the dialog so it doesn't grow off-screen.
        let preview: String = trimmed_notes.chars().take(800).collect();
        let elided = if trimmed_notes.chars().count() > 800 {
            "\n…"
        } else {
            ""
        };
        format!(
            "ESPHome Device Builder {} is available.\n\nYou currently have version {}.\n\nRelease notes:\n{}{}\n\nWould you like to download and install it now?",
            new, current, preview, elided
        )
    }
}
