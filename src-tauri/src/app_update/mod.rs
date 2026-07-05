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

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::MessageDialogKind;
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

            let msg = format_update_prompt(&current_version, &new_version, &notes);
            let confirmed = crate::dialog::confirm(
                app_handle,
                "Desktop Update Available",
                msg,
                "Update Now",
                "Later",
            )
            .await;

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
                let msg = format!("ESPHome Device Builder {} is the latest version.", current);
                crate::dialog::notice(
                    app_handle,
                    "No Updates Available",
                    msg,
                    MessageDialogKind::Info,
                )
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
pub async fn check_and_notify(app_handle: &AppHandle, tray_available: bool) -> NextStep {
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
            if let Err(e) = crate::update::notify_update_available(
                app_handle,
                "ESPHome Device Builder Update Available",
                &format!("Version {}", update.version),
                &update.current_version,
                tray_available,
            ) {
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
/// Thin dialog wrapper over [`apply_update_noninteractive`], which owns the
/// download→stop→install sequence (including the backend restore on a failed
/// install).
async fn apply_update(app_handle: &AppHandle, update: tauri_plugin_updater::Update) {
    let new_version = update.version.clone();

    match apply_update_noninteractive(app_handle, update, &|_, _| {}).await {
        Ok(()) => {
            // Always relaunch after a successful install rather than offering to
            // defer: the install replaced the .app bundle, and the running
            // process must be replaced by a fresh instance of it. On macOS the
            // relaunch must go through LaunchServices so the new process (and the
            // backend child it spawns) keeps the Local Network grant mDNS
            // discovery needs — see `platform::relaunch_for_update`. The dialog is
            // informational (single OK) just to explain the restart.
            let msg = format!(
                "ESPHome Device Builder {} has been installed.\n\nIt will now restart to apply the update.",
                new_version
            );
            crate::dialog::notice(app_handle, "Update Installed", msg, MessageDialogKind::Info)
                .await;

            info!("Relaunching to apply desktop update");
            crate::platform::relaunch_for_update(app_handle);
        }
        Err(e) => {
            show_error(app_handle, format!("Failed to update: {}", e)).await;
        }
    }
}

/// Non-interactive variant for the CLI `update` flow: the same
/// download→stop→install sequence as [`apply_update`], with progress reported
/// through the callback instead of dialogs, and no relaunch — the caller must
/// invoke `platform::relaunch_for_update` itself once it has flushed its reply
/// to the client, otherwise the relaunch would kill the control connection
/// before the client hears the outcome.
pub(crate) async fn apply_update_noninteractive(
    app_handle: &AppHandle,
    update: tauri_plugin_updater::Update,
    progress: crate::control::ops::Progress<'_>,
) -> Result<(), String> {
    let version = update.version.clone();

    progress("desktop", &format!("downloading desktop update {version}"));
    let bytes = download_update_bytes(&update)
        .await
        .map_err(|e| format!("download failed: {e}"))?;

    progress("desktop", "stopping the dashboard");
    stop_backend_for_install(app_handle).await;

    progress("desktop", &format!("installing desktop update {version}"));
    match install_update_bytes(update, bytes).await {
        Ok(()) => {
            info!("Desktop update {} installed", version);
            Ok(())
        }
        Err(e) => {
            error!("Desktop update install failed: {}", e);
            restore_backend(app_handle).await;
            Err(format!("install failed: {e}"))
        }
    }
}

/// Download the update payload with the backend still running. A failed
/// download must not disrupt the running dashboard, so the backend is only
/// stopped by the caller once the bytes are in hand and files are about to be
/// written.
async fn download_update_bytes(update: &tauri_plugin_updater::Update) -> Result<Vec<u8>, String> {
    let mut downloaded: u64 = 0;
    let mut last_logged = std::time::Instant::now();
    update
        .download(
            move |chunk, total| {
                downloaded = downloaded.saturating_add(chunk as u64);
                // Throttle progress logs to once per second.
                if last_logged.elapsed() >= Duration::from_secs(1) {
                    if let Some(total) = total {
                        info!("Downloading desktop update: {}/{} bytes", downloaded, total);
                    } else {
                        info!("Downloading desktop update: {} bytes", downloaded);
                    }
                    last_logged = std::time::Instant::now();
                }
            },
            || info!("Desktop update download complete"),
        )
        .await
        .map_err(|e| e.to_string())
}

/// Stop the backend before installing: the install overwrites the bundled
/// `python/` directory, and on Windows the live `python.exe` keeps those
/// files open (WinError 5) and holds port 6052, so the write fails and the
/// next launch can't bind. Reuses the same graceful `DaemonManager::stop()`
/// the ESPHome package-update path uses; best-effort, so proceed on error.
async fn stop_backend_for_install(app_handle: &AppHandle) {
    if let Some(state) = app_handle.try_state::<std::sync::Arc<crate::AppState>>() {
        info!("Stopping ESPHome backend before installing desktop update");
        if let Err(e) = state.daemon.stop().await {
            warn!("Error stopping backend before update: {}", e);
        }
    } else {
        warn!("App state unavailable; installing update without stopping backend");
    }
}

/// Install the downloaded bytes. `install` is synchronous and writes files, so
/// it runs off the async executor; the join error and the install error are
/// flattened so success and failure each have a single arm.
async fn install_update_bytes(
    update: tauri_plugin_updater::Update,
    bytes: Vec<u8>,
) -> Result<(), String> {
    info!("Installing desktop update…");
    match tokio::task::spawn_blocking(move || update.install(bytes)).await {
        Ok(install) => install.map_err(|e| e.to_string()),
        Err(join) => Err(format!("install task failed: {}", join)),
    }
}

/// Bring the backend back up when we're not restarting the whole app. We stop
/// it before installing, so on a failed install — where the bundle was not
/// replaced and the running process is still valid — without this the running
/// app would be left with no dashboard. (A successful install always relaunches,
/// so it never takes this path.) Best-effort.
async fn restore_backend(app_handle: &AppHandle) {
    if let Some(state) = app_handle.try_state::<std::sync::Arc<crate::AppState>>() {
        info!("Restarting ESPHome backend after desktop update");
        if let Err(e) = state.daemon.start().await {
            warn!("Failed to restart backend after update: {}", e);
        }
    }
}

async fn show_error(app_handle: &AppHandle, msg: String) {
    crate::dialog::notice(
        app_handle,
        "Update Check Failed",
        msg,
        MessageDialogKind::Error,
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn includes_both_versions() {
        let prompt = format_update_prompt("1.2.3", "1.3.0", "");
        assert!(prompt.contains("1.2.3"), "current version missing");
        assert!(prompt.contains("1.3.0"), "new version missing");
    }

    #[test]
    fn empty_notes_omits_release_notes_section() {
        let prompt = format_update_prompt("1.0.0", "2.0.0", "");
        assert!(!prompt.contains("Release notes:"), "empty notes: no header");
    }

    #[test]
    fn whitespace_only_notes_treated_as_empty() {
        // The notes are trimmed first, so a blank-but-non-empty body must take
        // the same path as truly empty notes — otherwise the dialog would show
        // an empty "Release notes:" section.
        let prompt = format_update_prompt("1.0.0", "2.0.0", "   \n\t  ");
        assert!(!prompt.contains("Release notes:"));
    }

    #[test]
    fn includes_notes_when_present() {
        let prompt = format_update_prompt("1.0.0", "2.0.0", "Fixed a crash on startup");
        assert!(prompt.contains("Release notes:"));
        assert!(prompt.contains("Fixed a crash on startup"));
    }

    #[test]
    fn short_notes_are_not_elided() {
        let prompt = format_update_prompt("1.0.0", "2.0.0", "short note");
        assert!(!prompt.contains('…'), "short notes elided");
    }

    #[test]
    fn notes_exactly_at_limit_are_not_elided() {
        // 800 chars is the boundary: `chars().count() > 800` is false, so the
        // full body is shown without an ellipsis.
        let notes = "a".repeat(800);
        let prompt = format_update_prompt("1.0.0", "2.0.0", &notes);
        assert!(!prompt.contains('…'), "800 chars elided");
        assert!(prompt.contains(&notes));
    }

    #[test]
    fn over_limit_notes_are_truncated_with_ellipsis() {
        // 801 chars trips the elision branch; only the first 800 are kept.
        let notes = "b".repeat(801);
        let prompt = format_update_prompt("1.0.0", "2.0.0", &notes);
        assert!(prompt.contains('…'), "over-limit not elided");
        assert!(!prompt.contains(&notes), "full body embedded");
        assert!(prompt.contains(&"b".repeat(800)), "first 800 dropped");
    }

    #[test]
    fn truncation_respects_char_boundaries_for_multibyte_notes() {
        // `chars().take(800)` counts Unicode scalar values, not bytes — a
        // body of 801 multi-byte chars must truncate to 800 chars without
        // panicking on a mid-codepoint split.
        let notes = "€".repeat(801);
        let prompt = format_update_prompt("1.0.0", "2.0.0", &notes);
        assert!(prompt.contains('…'));
        assert!(prompt.contains(&"€".repeat(800)));
        assert!(!prompt.contains(&"€".repeat(801)));
    }
}
