//! System tray menu
//!
//! Handles the system tray icon and context menu.

use anyhow::Result;
use std::sync::Arc;
use tauri::{
    async_runtime,
    menu::{Menu, MenuBuilder, MenuItem, MenuItemBuilder, SubmenuBuilder},
    AppHandle,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tracing::{error, info, warn};

use crate::settings::ReleaseChannel;
use crate::AppState;

/// Menu item IDs
mod ids {
    pub const OPEN_DASHBOARD: &str = "open_dashboard";
    pub const STATUS: &str = "status";
    pub const VERSION: &str = "version";
    pub const PORT: &str = "port";
    pub const CHECK_UPDATES: &str = "check_updates";
    pub const VIEW_LOGS: &str = "view_logs";
    pub const OPEN_CONFIG: &str = "open_config";
    pub const RESTART: &str = "restart";
    pub const QUIT: &str = "quit";

    // Release channel submenu items
    pub const CHANNEL_STABLE: &str = "channel_stable";
    pub const CHANNEL_BETA: &str = "channel_beta";
    pub const CHANNEL_DEV: &str = "channel_dev";
}

/// Build the tray menu
pub fn build_tray_menu(app_handle: &AppHandle, state: &Arc<AppState>) -> Result<Menu<tauri::Wry>> {
    let settings = async_runtime::block_on(state.settings.read());
    let status_text = if state.daemon.is_running() {
        "Status: Running"
    } else {
        "Status: Starting..."
    };

    // Create status item and store it for later updates
    let status_item = MenuItemBuilder::with_id(ids::STATUS, status_text)
        .enabled(false)
        .build(app_handle)?;
    let _ = STATUS_ITEM.set(status_item.clone());

    // Create version display item
    let version_text = match &settings.installed_version {
        Some(v) => format!("Version: {}", v),
        None => "Version: unknown".to_string(),
    };
    let version_item = MenuItemBuilder::with_id(ids::VERSION, version_text)
        .enabled(false)
        .build(app_handle)?;
    let _ = VERSION_ITEM.set(version_item.clone());

    // Create release channel items
    let current_channel = settings.release_channel;
    let channel_stable = MenuItemBuilder::with_id(ids::CHANNEL_STABLE, channel_label("Stable", current_channel == ReleaseChannel::Stable))
        .build(app_handle)?;
    let channel_beta = MenuItemBuilder::with_id(ids::CHANNEL_BETA, channel_label("Beta", current_channel == ReleaseChannel::Beta))
        .build(app_handle)?;
    let channel_dev = MenuItemBuilder::with_id(ids::CHANNEL_DEV, channel_label("Dev", current_channel == ReleaseChannel::Dev))
        .build(app_handle)?;

    // Store channel items for later updates
    let _ = CHANNEL_STABLE_ITEM.set(channel_stable.clone());
    let _ = CHANNEL_BETA_ITEM.set(channel_beta.clone());
    let _ = CHANNEL_DEV_ITEM.set(channel_dev.clone());

    let channel_submenu = SubmenuBuilder::with_id(app_handle, "release_channel", "Release Channel")
        .item(&channel_stable)
        .item(&channel_beta)
        .item(&channel_dev)
        .build()?;

    let menu = MenuBuilder::new(app_handle)
        .item(
            &MenuItemBuilder::with_id(ids::OPEN_DASHBOARD, "Open Dashboard")
                .accelerator("CmdOrCtrl+O")
                .build(app_handle)?,
        )
        .separator()
        .item(&status_item)
        .item(&version_item)
        .item(
            &MenuItemBuilder::with_id(ids::PORT, format!("Port: {}", settings.port))
                .enabled(false)
                .build(app_handle)?,
        )
        .separator()
        .item(&channel_submenu)
        .item(
            &MenuItemBuilder::with_id(ids::CHECK_UPDATES, "Check for Updates...")
                .build(app_handle)?,
        )
        .separator()
        .item(&MenuItemBuilder::with_id(ids::VIEW_LOGS, "View Logs...").build(app_handle)?)
        .item(
            &MenuItemBuilder::with_id(ids::OPEN_CONFIG, "Open Config Folder...").build(app_handle)?,
        )
        .item(
            &MenuItemBuilder::with_id(ids::RESTART, "Restart Dashboard").build(app_handle)?,
        )
        .separator()
        .item(&MenuItemBuilder::with_id(ids::QUIT, "Quit ESPHome").build(app_handle)?)
        .build()?;

    // Set up menu event handler
    let state_clone = state.clone();
    let app = app_handle.clone();
    app_handle.on_menu_event(move |app_handle, event| {
        handle_menu_event(app_handle, event.id().as_ref(), &state_clone, &app);
    });

    Ok(menu)
}

/// Status menu item stored globally for updates
static STATUS_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> = std::sync::OnceLock::new();

/// Version menu item stored globally for updates
static VERSION_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> = std::sync::OnceLock::new();

/// Release channel items stored globally for radio-button behavior
static CHANNEL_STABLE_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> =
    std::sync::OnceLock::new();
static CHANNEL_BETA_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> =
    std::sync::OnceLock::new();
static CHANNEL_DEV_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> =
    std::sync::OnceLock::new();

/// Update the tray status text
pub fn update_status(_app_handle: &AppHandle, running: bool) {
    let status_text = if running {
        "Status: Running"
    } else {
        "Status: Stopped"
    };

    if let Some(item) = STATUS_ITEM.get() {
        let _ = item.set_text(status_text);
    }
}

/// Update the version display in the tray menu.
pub fn update_version(version: &str) {
    if let Some(item) = VERSION_ITEM.get() {
        let _ = item.set_text(format!("Version: {}", version));
    }
}

/// Format a channel menu item label with a radio-button prefix.
fn channel_label(name: &str, selected: bool) -> String {
    if selected {
        format!("● {}", name)
    } else {
        format!("○ {}", name)
    }
}

/// Update the channel menu item labels to reflect the given channel
fn update_channel_checks(channel: ReleaseChannel) {
    if let Some(item) = CHANNEL_STABLE_ITEM.get() {
        let _ = item.set_text(channel_label("Stable", channel == ReleaseChannel::Stable));
    }
    if let Some(item) = CHANNEL_BETA_ITEM.get() {
        let _ = item.set_text(channel_label("Beta", channel == ReleaseChannel::Beta));
    }
    if let Some(item) = CHANNEL_DEV_ITEM.get() {
        let _ = item.set_text(channel_label("Dev", channel == ReleaseChannel::Dev));
    }
}

/// Re-detect the installed version and update the tray version display.
fn refresh_version_display(app_handle: &AppHandle) {
    let version = crate::update::get_installed_version(app_handle)
        .unwrap_or_else(|_| "unknown".to_string());
    update_version(&version);
}

/// Handle menu item clicks
fn handle_menu_event(app_handle: &AppHandle, id: &str, state: &Arc<AppState>, _app: &AppHandle) {
    match id {
        ids::OPEN_DASHBOARD => {
            let settings = async_runtime::block_on(state.settings.read());
            let url = format!("http://localhost:{}", settings.port);
            if let Err(e) = open::that(&url) {
                error!("Failed to open browser: {}", e);
            }
        }
        ids::CHECK_UPDATES => {
            let state = state.clone();
            let app = app_handle.clone();
            async_runtime::spawn(async move {
                let channel = {
                    let settings = state.settings.read().await;
                    settings.release_channel
                };

                // Check for updates and get version if user wants to update
                if let Some(version) = state.update_checker.check_for_user(&app, channel).await {
                    info!("User requested update to version {}", version);

                    // Stop the dashboard
                    update_status(&app, false);
                    if let Err(e) = state.daemon.stop().await {
                        error!("Failed to stop dashboard for update: {}", e);
                        let dialog_app = app.clone();
                        let msg = format!("Failed to stop dashboard: {}", e);
                        let _ = tokio::task::spawn_blocking(move || {
                            dialog_app
                                .dialog()
                                .message(msg)
                                .kind(MessageDialogKind::Error)
                                .title("Update Failed")
                                .blocking_show();
                        })
                        .await;
                        return;
                    }

                    // Perform the update
                    match state.update_checker.update_to(&app, &version, channel).await {
                        Ok(()) => {
                            info!("Update completed successfully");

                            // Update the version display in the tray menu
                            refresh_version_display(&app);

                            // Restart the dashboard
                            if let Err(e) = state.daemon.start().await {
                                error!("Failed to restart dashboard after update: {}", e);
                                let dialog_app = app.clone();
                                let msg = format!(
                                    "ESPHome updated to {}, but failed to restart dashboard: {}",
                                    version, e
                                );
                                let _ = tokio::task::spawn_blocking(move || {
                                    dialog_app
                                        .dialog()
                                        .message(msg)
                                        .kind(MessageDialogKind::Warning)
                                        .title("Update Partially Complete")
                                        .blocking_show();
                                })
                                .await;
                            } else {
                                update_status(&app, true);
                                let dialog_app = app.clone();
                                let msg = if channel == ReleaseChannel::Dev {
                                    "ESPHome has been updated to the latest dev version.".to_string()
                                } else {
                                    format!("ESPHome has been updated to version {}.", version)
                                };
                                let _ = tokio::task::spawn_blocking(move || {
                                    dialog_app
                                        .dialog()
                                        .message(msg)
                                        .kind(MessageDialogKind::Info)
                                        .title("Update Complete")
                                        .blocking_show();
                                })
                                .await;
                            }
                        }
                        Err(e) => {
                            error!("Update failed: {}", e);
                            let dialog_app = app.clone();
                            let msg = format!("Failed to update ESPHome: {}", e);
                            let _ = tokio::task::spawn_blocking(move || {
                                dialog_app
                                    .dialog()
                                    .message(msg)
                                    .kind(MessageDialogKind::Error)
                                    .title("Update Failed")
                                    .blocking_show();
                            })
                            .await;

                            // Try to restart dashboard anyway
                            if let Err(restart_err) = state.daemon.start().await {
                                error!("Failed to restart dashboard after failed update: {}", restart_err);
                            } else {
                                update_status(&app, true);
                            }
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
                // Read the current channel
                let old_channel = {
                    let settings = state.settings.read().await;
                    settings.release_channel
                };

                if new_channel == old_channel {
                    return;
                }

                // Confirm the channel switch with the user.
                // Run the blocking dialog on a dedicated thread so it cannot
                // starve the tokio runtime or the event-loop thread.
                let warning = if new_channel == ReleaseChannel::Dev {
                    "Warning: The dev channel installs ESPHome directly from GitHub and does NOT support automatic updates. You will need to manually check for updates.\n\n"
                } else {
                    ""
                };

                let dialog_app = app.clone();
                let msg = format!(
                    "{}Switch ESPHome from {} to {} channel?\n\nThis will stop the dashboard, install the appropriate version, and restart.",
                    warning, old_channel, new_channel
                );
                let confirmed = tokio::task::spawn_blocking(move || {
                    dialog_app
                        .dialog()
                        .message(msg)
                        .title("Switch Release Channel")
                        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                            "Switch".to_string(),
                            "Cancel".to_string(),
                        ))
                        .blocking_show()
                })
                .await
                .unwrap_or(false);

                if !confirmed {
                    // Revert the check marks
                    update_channel_checks(old_channel);
                    return;
                }

                // Update the check marks immediately to show the new selection
                update_channel_checks(new_channel);

                // Stop the dashboard
                update_status(&app, false);
                if let Err(e) = state.daemon.stop().await {
                    error!("Failed to stop dashboard for channel switch: {}", e);
                    let dialog_app = app.clone();
                    let msg = format!("Failed to stop dashboard: {}", e);
                    let _ = tokio::task::spawn_blocking(move || {
                        dialog_app
                            .dialog()
                            .message(msg)
                            .kind(MessageDialogKind::Error)
                            .title("Channel Switch Failed")
                            .blocking_show();
                    })
                    .await;
                    // Revert
                    update_channel_checks(old_channel);
                    return;
                }

                // Install the new channel's version
                match state.update_checker.switch_channel(&app, new_channel).await {
                    Ok(()) => {
                        info!("Switched to {} channel successfully", new_channel);

                        // Save the new channel to settings
                        {
                            let mut settings = state.settings.write().await;
                            settings.release_channel = new_channel;
                            if let Err(e) = settings.save(&app) {
                                warn!("Failed to save settings: {}", e);
                            }
                        }

                        // Update the version display in the tray menu
                        refresh_version_display(&app);

                        // Restart the dashboard
                        if let Err(e) = state.daemon.start().await {
                            error!("Failed to restart dashboard after channel switch: {}", e);
                            let dialog_app = app.clone();
                            let msg = format!(
                                "Switched to {} channel, but failed to restart dashboard: {}",
                                new_channel, e
                            );
                            let _ = tokio::task::spawn_blocking(move || {
                                dialog_app
                                    .dialog()
                                    .message(msg)
                                    .kind(MessageDialogKind::Warning)
                                    .title("Channel Switch Partially Complete")
                                    .blocking_show();
                            })
                            .await;
                        } else {
                            update_status(&app, true);
                            let dialog_app = app.clone();
                            let msg = format!(
                                "Successfully switched to the {} release channel.",
                                new_channel
                            );
                            let _ = tokio::task::spawn_blocking(move || {
                                dialog_app
                                    .dialog()
                                    .message(msg)
                                    .kind(MessageDialogKind::Info)
                                    .title("Channel Switched")
                                    .blocking_show();
                            })
                            .await;
                        }
                    }
                    Err(e) => {
                        error!("Channel switch failed: {}", e);
                        let dialog_app = app.clone();
                        let msg = format!("Failed to switch channel: {}", e);
                        let _ = tokio::task::spawn_blocking(move || {
                            dialog_app
                                .dialog()
                                .message(msg)
                                .kind(MessageDialogKind::Error)
                                .title("Channel Switch Failed")
                                .blocking_show();
                        })
                        .await;

                        // Revert settings
                        update_channel_checks(old_channel);

                        // Try to restart dashboard anyway
                        if let Err(restart_err) = state.daemon.start().await {
                            error!(
                                "Failed to restart dashboard after failed channel switch: {}",
                                restart_err
                            );
                        } else {
                            update_status(&app, true);
                        }
                    }
                }
            });
        }
        ids::VIEW_LOGS => {
            let logs_dir = state.daemon.logs_dir();
            if let Err(e) = open::that(logs_dir) {
                error!("Failed to open logs folder: {}", e);
            }
        }
        ids::OPEN_CONFIG => {
            let config_dir = state.daemon.config_dir();
            if let Err(e) = open::that(config_dir) {
                error!("Failed to open config folder: {}", e);
            }
        }
        ids::RESTART => {
            info!("Restarting ESPHome dashboard");
            let state = state.clone();
            async_runtime::spawn(async move {
                if let Err(e) = state.daemon.restart().await {
                    error!("Failed to restart daemon: {}", e);
                }
            });
        }
        ids::QUIT => {
            info!("Quit requested");
            let state = state.clone();
            let app = app_handle.clone();
            // Use block_on to ensure daemon stops before exit
            async_runtime::block_on(async move {
                if let Err(e) = state.daemon.stop().await {
                    error!("Error stopping daemon: {}", e);
                }
            });
            app.exit(0);
        }
        _ => {}
    }
}
