//! System-tray menu event dispatch.
//!
//! Handles clicks on the tray context-menu items. Split out of `mod.rs` to keep
//! that file under the file-size cap; the check-mark refresh helpers and menu-ID
//! constants this dispatch drives stay in the parent module and are reached via
//! `super::`.

use std::sync::Arc;

use tauri::{async_runtime, AppHandle};
use tauri_plugin_dialog::MessageDialogKind;
use tauri_plugin_notification::NotificationExt;
use tracing::{error, info, warn};

use crate::control::ops::{self, SwitchOutcome, UpdateGuard};
use crate::i18n::{t, t_with};
use crate::settings::{Backend, ReleaseChannel};
use crate::AppState;

use super::ids;
use super::{
    refresh_builder_version_display, refresh_version_display, update_backend_checks,
    update_channel_checks,
};

pub(super) fn handle_menu_event(app_handle: &AppHandle, id: &str, state: &Arc<AppState>) {
    /// Acquire the `UpdateGuard` or log and `return` from the spawned task.
    /// Collapses the acquire-or-bail boilerplate shared by the three multi-step
    /// menu arms while preserving the `return`-in-closure control flow.
    macro_rules! guard_or_return {
        ($state:expr, $what:expr) => {
            match UpdateGuard::try_acquire($state.update_in_flight.clone()) {
                Some(g) => g,
                None => {
                    info!("Update/switch already in progress; ignoring {}", $what);
                    return;
                }
            }
        };
    }

    match id {
        ids::OPEN_DASHBOARD => {
            let settings = async_runtime::block_on(state.settings.read());
            crate::open_dashboard(settings.port);
        }
        ids::STARTUP_ENABLE | ids::STARTUP_DISABLE => {
            let enable = id == ids::STARTUP_ENABLE;
            let state = state.clone();
            let app = app_handle.clone();
            async_runtime::spawn(async move {
                ops::set_launch_at_startup(&app, &state, enable).await;
            });
        }
        ids::CHECK_UPDATES => {
            let state = state.clone();
            let app = app_handle.clone();
            async_runtime::spawn(async move {
                let _guard = guard_or_return!(state, "Check for Updates");
                // Always check the desktop app first. Installing a self-update
                // replaces the bundled `python/` directory, which would wipe
                // any pip bump we do here. If the user accepts the app update
                // (or it errors mid-install), skip the Python checks; if they
                // decline or there's no app update, fall through.
                if crate::app_update::check_for_user(&app, false).await
                    == crate::app_update::NextStep::Skip
                {
                    return;
                }

                let (channel, backend) = {
                    let settings = state.settings.read().await;
                    (settings.release_channel, settings.backend)
                };

                // Check for updates and get version if user wants to update
                if let Some(version) = state.update_checker.check_for_user(&app, channel).await {
                    info!("User requested update to version {}", version);

                    // Stop the dashboard
                    if let Err(e) = state.daemon.stop().await {
                        error!("Failed to stop backend for update: {}", e);
                        crate::dialog::notice(
                            &app,
                            &t("update.update_failed_title"),
                            t_with("errors.stop_dashboard_failed", &[("error", &e.to_string())]),
                            MessageDialogKind::Error,
                        )
                        .await;
                        return;
                    }

                    // Perform the update
                    match state
                        .update_checker
                        .update_to(&app, &version, channel)
                        .await
                    {
                        Ok(()) => {
                            info!("Update completed successfully");

                            // Update the version display in the tray menu, off
                            // the async executor (detection spawns a Python
                            // subprocess) — mirrors the device-builder arm below.
                            let refresh_app = app.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                refresh_version_display(&refresh_app)
                            })
                            .await;

                            // Restart the dashboard
                            if let Err(e) = state.daemon.start().await {
                                error!("Failed to restart backend after update: {}", e);
                                crate::dialog::notice(
                                    &app,
                                    &t("update.update_partial_title"),
                                    t_with(
                                        "update.esphome_partial",
                                        &[("version", version.as_str()), ("error", &e.to_string())],
                                    ),
                                    MessageDialogKind::Warning,
                                )
                                .await;
                            } else {
                                let msg = if channel == ReleaseChannel::Dev {
                                    t("update.esphome_updated_dev")
                                } else {
                                    t_with("update.esphome_updated", &[("version", &version)])
                                };
                                crate::dialog::notice(
                                    &app,
                                    &t("update.update_complete_title"),
                                    msg,
                                    MessageDialogKind::Info,
                                )
                                .await;
                            }
                        }
                        Err(e) => {
                            error!("Update failed: {}", e);
                            crate::dialog::notice(
                                &app,
                                &t("update.update_failed_title"),
                                t_with(
                                    "update.esphome_update_failed",
                                    &[("error", &e.to_string())],
                                ),
                                MessageDialogKind::Error,
                            )
                            .await;

                            // Try to restart dashboard anyway
                            if let Err(restart_err) = state.daemon.start().await {
                                error!(
                                    "Failed to restart backend after failed update: {}",
                                    restart_err
                                );
                            }
                        }
                    }
                }

                // Also check `esphome-device-builder`, independent of the
                // ESPHome release channel.
                let Some(builder_version) = state
                    .update_checker
                    .check_device_builder_for_user(&app, backend)
                    .await
                else {
                    return;
                };
                info!(
                    "User requested device-builder update to version {}",
                    builder_version
                );

                if let Err(e) = state.daemon.stop().await {
                    error!("Failed to stop backend for device-builder update: {}", e);
                    crate::dialog::notice(
                        &app,
                        &t("update.update_failed_title"),
                        t_with("errors.stop_backend_failed", &[("error", &e.to_string())]),
                        MessageDialogKind::Error,
                    )
                    .await;
                    return;
                }

                match state
                    .update_checker
                    .install_device_builder(&app, backend)
                    .await
                {
                    Ok(()) => {
                        info!("Device builder updated successfully to {}", builder_version);

                        // Refresh the device-builder version display in the tray menu
                        refresh_builder_version_display(&app).await;

                        if let Err(e) = state.daemon.start().await {
                            error!(
                                "Failed to restart backend after device-builder update: {}",
                                e
                            );
                            crate::dialog::notice(
                                &app,
                                &t("update.update_partial_title"),
                                t_with(
                                    "update.builder_partial",
                                    &[
                                        ("version", builder_version.as_str()),
                                        ("error", &e.to_string()),
                                    ],
                                ),
                                MessageDialogKind::Warning,
                            )
                            .await;
                        } else {
                            crate::dialog::notice(
                                &app,
                                &t("update.update_complete_title"),
                                t_with("update.builder_updated", &[("version", &builder_version)]),
                                MessageDialogKind::Info,
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        error!("Device-builder update failed: {}", e);
                        crate::dialog::notice(
                            &app,
                            &t("update.update_failed_title"),
                            t_with("update.builder_update_failed", &[("error", &e.to_string())]),
                            MessageDialogKind::Error,
                        )
                        .await;

                        // Try to restart backend anyway
                        if let Err(restart_err) = state.daemon.start().await {
                            error!(
                                "Failed to restart backend after failed device-builder update: {}",
                                restart_err
                            );
                        }
                    }
                }
            });
        }
        ids::CHANNEL_STABLE | ids::CHANNEL_BETA | ids::CHANNEL_DEV => {
            let new_channel = match id {
                ids::CHANNEL_STABLE => ReleaseChannel::Stable,
                ids::CHANNEL_BETA => ReleaseChannel::Beta,
                ids::CHANNEL_DEV => ReleaseChannel::Dev,
                _ => unreachable!(),
            };

            let state = state.clone();
            let app = app_handle.clone();
            async_runtime::spawn(async move {
                let guard = guard_or_return!(state, "channel switch");
                // Read the current channel
                let old_channel = {
                    let settings = state.settings.read().await;
                    settings.release_channel
                };

                if new_channel == old_channel {
                    return;
                }

                // Confirm the channel switch with the user.
                let prompt = t_with(
                    "switch_channel.prompt",
                    &[
                        ("old", &old_channel.to_string()),
                        ("new", &new_channel.to_string()),
                    ],
                );
                let msg = if new_channel == ReleaseChannel::Dev {
                    format!("{}\n\n{}", t("switch_channel.dev_warning"), prompt)
                } else {
                    prompt
                };
                let confirmed = crate::dialog::confirm(
                    &app,
                    &t("switch_channel.title"),
                    msg,
                    &t("common.switch"),
                    &t("common.cancel"),
                )
                .await;

                if !confirmed {
                    // Revert the check marks
                    update_channel_checks(old_channel);
                    return;
                }

                // The stop→install→persist→start sequence (including label
                // updates and their failure-path reverts) lives in ops so the
                // CLI drives the exact same code; the tray adds the dialogs.
                match ops::switch_release_channel(&app, &state, new_channel, &guard, &|_, _| {})
                    .await
                {
                    SwitchOutcome::Unchanged => {}
                    SwitchOutcome::Success { .. } => {
                        let msg = t_with(
                            "switch_channel.switched",
                            &[("channel", &new_channel.to_string())],
                        );
                        crate::dialog::notice(
                            &app,
                            &t("switch_channel.switched_title"),
                            msg,
                            MessageDialogKind::Info,
                        )
                        .await;
                    }
                    SwitchOutcome::StopFailed(e) => {
                        crate::dialog::notice(
                            &app,
                            &t("switch_channel.failed_title"),
                            t_with("errors.stop_dashboard_failed", &[("error", &e.to_string())]),
                            MessageDialogKind::Error,
                        )
                        .await;
                    }
                    SwitchOutcome::InstallFailed { error, .. } => {
                        crate::dialog::notice(
                            &app,
                            &t("switch_channel.failed_title"),
                            t_with("switch_channel.failed", &[("error", &error.to_string())]),
                            MessageDialogKind::Error,
                        )
                        .await;
                    }
                    SwitchOutcome::StartFailed(e) => {
                        crate::dialog::notice(
                            &app,
                            &t("switch_channel.partial_title"),
                            t_with(
                                "switch_channel.partial",
                                &[
                                    ("channel", &new_channel.to_string()),
                                    ("error", &e.to_string()),
                                ],
                            ),
                            MessageDialogKind::Warning,
                        )
                        .await;
                    }
                }
            });
        }
        ids::BACKEND_BUILDER_STABLE | ids::BACKEND_BUILDER_BETA => {
            let new_backend = match id {
                ids::BACKEND_BUILDER_STABLE => Backend::BuilderStable,
                ids::BACKEND_BUILDER_BETA => Backend::BuilderBeta,
                _ => unreachable!(),
            };

            let state = state.clone();
            let app = app_handle.clone();
            async_runtime::spawn(async move {
                let guard = guard_or_return!(state, "backend switch");
                let old_backend = {
                    let settings = state.settings.read().await;
                    settings.backend
                };

                if new_backend == old_backend {
                    return;
                }

                // Confirm the switch with the user.
                let msg = t_with(
                    "switch_backend.prompt",
                    &[("backend", &new_backend.to_string())],
                );
                let confirmed = crate::dialog::confirm(
                    &app,
                    &t("switch_backend.title"),
                    msg,
                    &t("common.switch"),
                    &t("common.cancel"),
                )
                .await;

                if !confirmed {
                    update_backend_checks(old_backend);
                    return;
                }

                // The stop→install→persist→start→wait sequence (including
                // label updates and their failure-path reverts) lives in ops
                // so the CLI drives the exact same code; the tray adds the
                // dialogs and the readiness notification.
                match ops::switch_backend(&app, &state, new_backend, &guard, &|_, _| {}).await {
                    SwitchOutcome::Unchanged => {}
                    SwitchOutcome::Success { ready } => {
                        let body = if ready {
                            t_with(
                                "switch_backend.ready",
                                &[("backend", &new_backend.to_string())],
                            )
                        } else {
                            t_with(
                                "switch_backend.not_ready",
                                &[("backend", &new_backend.to_string())],
                            )
                        };
                        if let Err(e) = app
                            .notification()
                            .builder()
                            .title(t("switch_backend.switched_title"))
                            .body(body)
                            .show()
                        {
                            warn!("Failed to show backend-switch notification: {}", e);
                        }
                    }
                    SwitchOutcome::StopFailed(e) => {
                        crate::dialog::notice(
                            &app,
                            &t("switch_backend.failed_title"),
                            t_with("errors.stop_backend_failed", &[("error", &e.to_string())]),
                            MessageDialogKind::Error,
                        )
                        .await;
                    }
                    SwitchOutcome::InstallFailed { error, .. } => {
                        crate::dialog::notice(
                            &app,
                            &t("switch_backend.failed_title"),
                            t_with(
                                "switch_backend.install_failed",
                                &[("error", &error.to_string())],
                            ),
                            MessageDialogKind::Error,
                        )
                        .await;
                    }
                    SwitchOutcome::StartFailed(e) => {
                        crate::dialog::notice(
                            &app,
                            &t("switch_backend.failed_title"),
                            t_with("switch_backend.start_failed", &[("error", &e.to_string())]),
                            MessageDialogKind::Error,
                        )
                        .await;
                    }
                }
            });
        }
        ids::VIEW_LOGS => {
            let logs_dir = state.daemon.logs_dir();
            if let Err(e) = open::that_detached(logs_dir) {
                error!("Failed to open logs folder: {}", e);
            }
        }
        ids::OPEN_CONFIG => {
            let config_dir = state.daemon.config_dir();
            if let Err(e) = open::that_detached(config_dir) {
                error!("Failed to open config folder: {}", e);
            }
        }
        ids::RESTART => {
            let state = state.clone();
            async_runtime::spawn(async move {
                // restart() is a stop()->start() sequence, so it must hold the
                // same re-entrancy guard as the channel/backend switch arms.
                // Without it a Restart click during an in-flight switch can run
                // start() with the OLD backend before the switch persists the
                // new one, leaving the running process out of sync with the
                // saved settings and tray radio state.
                let guard = guard_or_return!(state, "restart");
                info!("Restarting ESPHome backend");
                if let Err(e) = ops::restart_daemon(&state, false, &guard, &|_, _| {}).await {
                    error!("Failed to restart daemon: {}", e);
                }
            });
        }
        ids::QUIT => {
            // Refuse to tear the app down while an update/switch is mid-flight:
            // exiting now would orphan a pip install mid-write and corrupt the
            // site-packages tree (the same reason the update arms hold the
            // guard). The click is ignored with a log line if a sequence is in
            // progress. Keep the flag held for the rest of this process's life
            // (like the Update arm): dropping it when this arm returns would
            // reopen the window during the async teardown (`daemon.stop()`) for
            // a concurrent update to start a pip install the exit then orphans.
            let guard = guard_or_return!(state, "Quit");
            info!("Quit requested");
            std::mem::forget(guard);
            // Delegate cleanup to the RunEvent::ExitRequested handler in
            // lib.rs so the shutdown sequence lives in exactly one place.
            app_handle.exit(0);
        }
        _ => {}
    }
}
