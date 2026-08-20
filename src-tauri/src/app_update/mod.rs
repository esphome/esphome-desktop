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

use tauri::utils::config::BundleType;
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::MessageDialogKind;
use tauri_plugin_updater::UpdaterExt;
use tracing::{debug, error, info, warn};

use crate::i18n::{t, t_with};

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

/// The package tool a deb/rpm-typed install needs for the updater's install
/// step, when that tool is missing from `PATH` — i.e. the reason this binary
/// must not attempt a self-update.
///
/// tauri-plugin-updater dispatches its install by the bundle type baked into
/// the binary at packaging time: a deb runs `dpkg -i`, an rpm runs `rpm -U`,
/// each behind a pkexec/zenity/sudo elevation chain. A binary repackaged onto
/// a system without that tool — the AUR package extracts our amd64 `.deb`, and
/// Arch has no `dpkg` — would download the full update and then fail every
/// rung of that chain, showing an elevation prompt on the way down. Split from
/// [`self_update_blocked`] so the decision is testable on every host.
fn self_update_blocked_tool(
    bundle: Option<BundleType>,
    tool_present: impl Fn(&str) -> bool,
) -> Option<&'static str> {
    let tool = match bundle {
        Some(BundleType::Deb) => "dpkg",
        Some(BundleType::Rpm) => "rpm",
        // AppImage/Msi/Nsis/App install without a package tool, and `None`
        // (an unpackaged dev build) keeps today's behavior.
        _ => return None,
    };
    (!tool_present(tool)).then_some(tool)
}

/// [`self_update_blocked_tool`] for the running binary and the real `PATH`:
/// `Some(tool)` names the missing package tool when a self-update must not be
/// attempted, `None` means the updater may proceed.
fn self_update_blocked() -> Option<&'static str> {
    self_update_blocked_tool(
        tauri::utils::platform::bundle_type(),
        crate::platform::executable_on_path,
    )
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
            show_error(
                app_handle,
                t_with(
                    "app_update.updater_unavailable",
                    &[("error", &e.to_string())],
                ),
            )
            .await;
            return NextStep::Continue;
        }
    };

    match updater.check().await {
        Ok(Some(update)) => {
            info!(
                "Desktop update available: {} (current: {})",
                update.version, update.current_version
            );

            if let Some(tool) = self_update_blocked() {
                info!(
                    "Not offering to install {}: this install has no `{}`",
                    update.version, tool
                );
                crate::dialog::notice(
                    app_handle,
                    &t("app_update.available_title"),
                    t_with(
                        "app_update.package_manager_only",
                        &[("new", &update.version), ("tool", tool)],
                    ),
                    MessageDialogKind::Info,
                )
                .await;
                // The update exists but cannot be installed from here, so the
                // bundled Python is not about to roll — keep going to the
                // ESPHome checks, same as a declined update.
                return NextStep::Continue;
            }

            let new_version = update.version.clone();
            let current_version = update.current_version.clone();
            let notes = update.body.clone().unwrap_or_default();

            let msg = format_update_prompt(&current_version, &new_version, &notes);
            let confirmed = crate::dialog::confirm(
                app_handle,
                &t("app_update.available_title"),
                msg,
                &t("common.update_now"),
                &t("common.later"),
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
                let msg = t_with("app_update.latest", &[("version", &current)]);
                crate::dialog::notice(
                    app_handle,
                    &t("update.none_title"),
                    msg,
                    MessageDialogKind::Info,
                )
                .await;
            }
            NextStep::Continue
        }
        Err(e) => {
            warn!("Desktop update check failed: {}", e);
            show_error(
                app_handle,
                t_with("update.check_failed", &[("error", &e.to_string())]),
            )
            .await;
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
            // A blocked update (a deb/rpm repackage without its package tool —
            // the AUR case) cannot be installed from this binary: the
            // notification's hint must point at the package manager rather
            // than an in-app flow that cannot perform the update, and the pip
            // updates won't be overwritten by it, so the loop must keep
            // checking them rather than skipping forever.
            let blocked = self_update_blocked().is_some();
            let hint = if blocked {
                crate::update::NotificationHint::PackageManager
            } else {
                crate::update::NotificationHint::InApp { tray_available }
            };
            if let Err(e) = crate::update::notify_update_available(
                app_handle,
                &t_with(
                    "update.notification_title",
                    &[("component", "ESPHome Device Builder")],
                ),
                &t_with(
                    "app_update.notification_subject",
                    &[("version", &update.version)],
                ),
                &update.current_version,
                hint,
            ) {
                error!("Failed to show desktop-update notification: {}", e);
            }
            if blocked {
                NextStep::Continue
            } else {
                NextStep::Skip
            }
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
        Ok(AppUpdateOutcome::Installed) => {
            // Always relaunch after a successful install rather than offering to
            // defer: the install replaced the .app bundle, and the running
            // process must be replaced by a fresh instance of it. On macOS the
            // relaunch must go through LaunchServices so the new process (and the
            // backend child it spawns) keeps the Local Network grant mDNS
            // discovery needs — see `platform::relaunch_for_update`. The dialog is
            // informational (single OK) just to explain the restart.
            let msg = t_with("app_update.installed_body", &[("version", &new_version)]);
            crate::dialog::notice(
                app_handle,
                &t("app_update.installed_title"),
                msg,
                MessageDialogKind::Info,
            )
            .await;

            info!("Relaunching to apply desktop update");
            crate::platform::relaunch_for_update(app_handle);
        }
        Ok(AppUpdateOutcome::ExternallyManaged(tool)) => {
            // Only reachable when the tool disappeared between
            // `check_for_user`'s guard and the user confirming — still a
            // normal condition, so the same informational notice as the
            // guard, never the red failure dialog.
            crate::dialog::notice(
                app_handle,
                &t("app_update.available_title"),
                t_with(
                    "app_update.package_manager_only",
                    &[("new", &new_version), ("tool", tool)],
                ),
                MessageDialogKind::Info,
            )
            .await;
        }
        Err(e) => {
            show_error(
                app_handle,
                t_with("app_update.update_failed", &[("error", &e)]),
            )
            .await;
        }
    }
}

/// How [`apply_update_noninteractive`] finished, short of an actual failure.
///
/// `ExternallyManaged` is a normal condition, not an error — carrying it in
/// `Err` would force every caller to parse a failure back into a note, and
/// would render a red "Failed to update" dialog for something that is merely
/// "not ours to install". `Err` stays reserved for downloads and installs
/// that actually broke.
pub(crate) enum AppUpdateOutcome {
    /// Downloaded and installed; the caller must relaunch the app.
    Installed,
    /// Not attempted: this install updates through the system package manager
    /// and the named tool is missing ([`self_update_blocked`]). Nothing was
    /// downloaded and the backend was never stopped.
    ExternallyManaged(&'static str),
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
) -> Result<AppUpdateOutcome, String> {
    let version = update.version.clone();

    // The one decision point for "can this binary install its own update":
    // nothing may reach the download when the install step's package tool is
    // missing (see [`self_update_blocked`]) — the plugin would fetch the full
    // payload and then fail its pkexec/sudo install chain.
    if let Some(tool) = self_update_blocked() {
        return Ok(AppUpdateOutcome::ExternallyManaged(tool));
    }

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
            Ok(AppUpdateOutcome::Installed)
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
        &t("update.check_failed_title"),
        msg,
        MessageDialogKind::Error,
    )
    .await;
}

fn format_update_prompt(current: &str, new: &str, notes: &str) -> String {
    let trimmed_notes = notes.trim();
    if trimmed_notes.is_empty() {
        t_with("app_update.prompt", &[("new", new), ("current", current)])
    } else {
        // Keep release notes short in the dialog so it doesn't grow off-screen.
        let mut preview: String = trimmed_notes.chars().take(800).collect();
        if trimmed_notes.chars().count() > 800 {
            preview.push_str("\n…");
        }
        t_with(
            "app_update.prompt_with_notes",
            &[("new", new), ("current", current), ("notes", &preview)],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deb_and_rpm_without_their_tool_are_blocked_and_name_it() {
        assert_eq!(
            self_update_blocked_tool(Some(BundleType::Deb), |_| false),
            Some("dpkg")
        );
        assert_eq!(
            self_update_blocked_tool(Some(BundleType::Rpm), |_| false),
            Some("rpm")
        );
    }

    #[test]
    fn deb_and_rpm_with_their_tool_are_allowed() {
        assert_eq!(
            self_update_blocked_tool(Some(BundleType::Deb), |t| t == "dpkg"),
            None
        );
        assert_eq!(
            self_update_blocked_tool(Some(BundleType::Rpm), |t| t == "rpm"),
            None
        );
    }

    #[test]
    fn non_package_bundles_never_consult_path() {
        // AppImage/Msi/Nsis/App/Dmg install without a package tool, and `None`
        // is an unpackaged dev build; the panicking closure proves the PATH
        // question is never even asked for these, so the guard cannot change
        // their behavior.
        for bundle in [
            None,
            Some(BundleType::AppImage),
            Some(BundleType::Msi),
            Some(BundleType::Nsis),
            Some(BundleType::App),
            Some(BundleType::Dmg),
        ] {
            assert_eq!(
                self_update_blocked_tool(bundle, |tool| panic!("asked PATH about {tool}")),
                None
            );
        }
    }

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
