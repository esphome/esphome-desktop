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

use crate::settings::{Backend, ReleaseChannel};
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

    // Backend submenu items
    pub const BACKEND_CLASSIC: &str = "backend_classic";
    pub const BACKEND_BUILDER_STABLE: &str = "backend_builder_stable";
    pub const BACKEND_BUILDER_BETA: &str = "backend_builder_beta";
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
    let channel_stable = MenuItemBuilder::with_id(ids::CHANNEL_STABLE, radio_label("Stable", current_channel == ReleaseChannel::Stable))
        .build(app_handle)?;
    let channel_beta = MenuItemBuilder::with_id(ids::CHANNEL_BETA, radio_label("Beta", current_channel == ReleaseChannel::Beta))
        .build(app_handle)?;
    let channel_dev = MenuItemBuilder::with_id(ids::CHANNEL_DEV, radio_label("Dev", current_channel == ReleaseChannel::Dev))
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

    // Backend submenu items
    let current_backend = settings.backend;
    let backend_classic = MenuItemBuilder::with_id(
        ids::BACKEND_CLASSIC,
        radio_label("Classic ESPHome Dashboard", current_backend == Backend::Classic),
    )
    .build(app_handle)?;
    let backend_builder_stable = MenuItemBuilder::with_id(
        ids::BACKEND_BUILDER_STABLE,
        radio_label(
            "ESPHome Builder (stable)",
            current_backend == Backend::BuilderStable,
        ),
    )
    .enabled(false) // TODO: remove once a stable release of esphome-device-builder is out
    .build(app_handle)?;
    let backend_builder_beta = MenuItemBuilder::with_id(
        ids::BACKEND_BUILDER_BETA,
        radio_label(
            "ESPHome Builder (beta)",
            current_backend == Backend::BuilderBeta,
        ),
    )
    .build(app_handle)?;

    let _ = BACKEND_CLASSIC_ITEM.set(backend_classic.clone());
    let _ = BACKEND_BUILDER_STABLE_ITEM.set(backend_builder_stable.clone());
    let _ = BACKEND_BUILDER_BETA_ITEM.set(backend_builder_beta.clone());

    let backend_submenu = SubmenuBuilder::with_id(app_handle, "backend", "Backend")
        .item(&backend_classic)
        .item(&backend_builder_stable)
        .item(&backend_builder_beta)
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
        .item(&backend_submenu)
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

/// Backend menu items stored globally for radio-button behavior
static BACKEND_CLASSIC_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> =
    std::sync::OnceLock::new();
static BACKEND_BUILDER_STABLE_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> =
    std::sync::OnceLock::new();
static BACKEND_BUILDER_BETA_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> =
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

/// Format a radio-style menu item label with a selection prefix.
fn radio_label(name: &str, selected: bool) -> String {
    if selected {
        format!("● {}", name)
    } else {
        format!("○ {}", name)
    }
}

/// Update the channel menu item labels to reflect the given channel
fn update_channel_checks(channel: ReleaseChannel) {
    if let Some(item) = CHANNEL_STABLE_ITEM.get() {
        let _ = item.set_text(radio_label("Stable", channel == ReleaseChannel::Stable));
    }
    if let Some(item) = CHANNEL_BETA_ITEM.get() {
        let _ = item.set_text(radio_label("Beta", channel == ReleaseChannel::Beta));
    }
    if let Some(item) = CHANNEL_DEV_ITEM.get() {
        let _ = item.set_text(radio_label("Dev", channel == ReleaseChannel::Dev));
    }
}

/// Update the backend menu item labels to reflect the given backend.
fn update_backend_checks(backend: Backend) {
    if let Some(item) = BACKEND_CLASSIC_ITEM.get() {
        let _ = item.set_text(radio_label(
            "Classic ESPHome Dashboard",
            backend == Backend::Classic,
        ));
    }
    if let Some(item) = BACKEND_BUILDER_STABLE_ITEM.get() {
        let _ = item.set_text(radio_label(
            "ESPHome Builder (stable)",
            backend == Backend::BuilderStable,
        ));
    }
    if let Some(item) = BACKEND_BUILDER_BETA_ITEM.get() {
        let _ = item.set_text(radio_label(
            "ESPHome Builder (beta)",
            backend == Backend::BuilderBeta,
        ));
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
                let (channel, backend) = {
                    let settings = state.settings.read().await;
                    (settings.release_channel, settings.backend)
                };

                // Check for updates and get version if user wants to update
                if let Some(version) = state.update_checker.check_for_user(&app, channel).await {
                    info!("User requested update to version {}", version);

                    // Stop the dashboard
                    update_status(&app, false);
                    if let Err(e) = state.daemon.stop().await {
                        error!("Failed to stop backend for update: {}", e);
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
                                error!("Failed to restart backend after update: {}", e);
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
                                error!("Failed to restart backend after failed update: {}", restart_err);
                            } else {
                                update_status(&app, true);
                            }
                        }
                    }
                }

                // Also check `esphome-device-builder` when it's the active
                // backend. This is independent of the ESPHome release channel.
                if backend.is_builder() {
                    if let Some(builder_version) = state
                        .update_checker
                        .check_device_builder_for_user(&app, backend)
                        .await
                    {
                        info!(
                            "User requested device-builder update to version {}",
                            builder_version
                        );

                        update_status(&app, false);
                        if let Err(e) = state.daemon.stop().await {
                            error!("Failed to stop backend for device-builder update: {}", e);
                            let dialog_app = app.clone();
                            let msg = format!("Failed to stop backend: {}", e);
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

                        match state
                            .update_checker
                            .install_device_builder(&app, backend)
                            .await
                        {
                            Ok(()) => {
                                info!(
                                    "Device builder updated successfully to {}",
                                    builder_version
                                );

                                if let Err(e) = state.daemon.start().await {
                                    error!(
                                        "Failed to restart backend after device-builder update: {}",
                                        e
                                    );
                                    let dialog_app = app.clone();
                                    let msg = format!(
                                        "Device builder updated to {}, but failed to restart backend: {}",
                                        builder_version, e
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
                                    let msg = format!(
                                        "ESPHome Device Builder has been updated to version {}.",
                                        builder_version
                                    );
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
                                error!("Device-builder update failed: {}", e);
                                let dialog_app = app.clone();
                                let msg = format!("Failed to update ESPHome Device Builder: {}", e);
                                let _ = tokio::task::spawn_blocking(move || {
                                    dialog_app
                                        .dialog()
                                        .message(msg)
                                        .kind(MessageDialogKind::Error)
                                        .title("Update Failed")
                                        .blocking_show();
                                })
                                .await;

                                // Try to restart backend anyway
                                if let Err(restart_err) = state.daemon.start().await {
                                    error!(
                                        "Failed to restart backend after failed device-builder update: {}",
                                        restart_err
                                    );
                                } else {
                                    update_status(&app, true);
                                }
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
                    error!("Failed to stop backend for channel switch: {}", e);
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
                            error!("Failed to restart backend after channel switch: {}", e);
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
                                "Failed to restart backend after failed channel switch: {}",
                                restart_err
                            );
                        } else {
                            update_status(&app, true);
                        }
                    }
                }
            });
        }
        ids::BACKEND_CLASSIC | ids::BACKEND_BUILDER_STABLE | ids::BACKEND_BUILDER_BETA => {
            let new_backend = match id {
                ids::BACKEND_CLASSIC => Backend::Classic,
                ids::BACKEND_BUILDER_STABLE => Backend::BuilderStable,
                ids::BACKEND_BUILDER_BETA => Backend::BuilderBeta,
                _ => unreachable!(),
            };

            let state = state.clone();
            let app = app_handle.clone();
            async_runtime::spawn(async move {
                let old_backend = {
                    let settings = state.settings.read().await;
                    settings.backend
                };

                if new_backend == old_backend {
                    return;
                }

                // Confirm the switch with the user.
                let dialog_app = app.clone();
                let msg = if new_backend.is_builder() {
                    format!(
                        "Switch to {}?\n\n\
                         This will install the `esphome-device-builder` Python package, \
                         stop the current backend, and restart with the new one.",
                        new_backend
                    )
                } else {
                    "Switch back to the classic ESPHome dashboard?\n\n\
                     This will stop the device builder and restart with the dashboard."
                        .to_string()
                };
                let confirmed = tokio::task::spawn_blocking(move || {
                    dialog_app
                        .dialog()
                        .message(msg)
                        .title("Switch Backend")
                        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
                            "Switch".to_string(),
                            "Cancel".to_string(),
                        ))
                        .blocking_show()
                })
                .await
                .unwrap_or(false);

                if !confirmed {
                    update_backend_checks(old_backend);
                    return;
                }

                // Update the check marks immediately to show the new selection
                update_backend_checks(new_backend);

                // Stop the current backend
                update_status(&app, false);
                if let Err(e) = state.daemon.stop().await {
                    error!("Failed to stop daemon for backend switch: {}", e);
                    let dialog_app = app.clone();
                    let msg = format!("Failed to stop backend: {}", e);
                    let _ = tokio::task::spawn_blocking(move || {
                        dialog_app
                            .dialog()
                            .message(msg)
                            .kind(MessageDialogKind::Error)
                            .title("Backend Switch Failed")
                            .blocking_show();
                    })
                    .await;
                    update_backend_checks(old_backend);
                    return;
                }

                // When switching to a builder variant, install/upgrade the package first.
                if new_backend.is_builder() {
                    if let Err(e) = state
                        .update_checker
                        .install_device_builder(&app, new_backend)
                        .await
                    {
                        error!("Failed to install esphome-device-builder: {}", e);
                        let dialog_app = app.clone();
                        let msg = format!("Failed to install esphome-device-builder: {}", e);
                        let _ = tokio::task::spawn_blocking(move || {
                            dialog_app
                                .dialog()
                                .message(msg)
                                .kind(MessageDialogKind::Error)
                                .title("Backend Switch Failed")
                                .blocking_show();
                        })
                        .await;
                        update_backend_checks(old_backend);
                        // Try to restart the original backend.
                        if let Err(restart_err) = state.daemon.start().await {
                            error!(
                                "Failed to restart backend after failed switch: {}",
                                restart_err
                            );
                        } else {
                            update_status(&app, true);
                        }
                        return;
                    }
                }

                // Apply the new backend to the daemon and persist it.
                state.daemon.set_use_device_builder(new_backend.is_builder());
                {
                    let mut settings = state.settings.write().await;
                    settings.backend = new_backend;
                    if let Err(e) = settings.save(&app) {
                        warn!("Failed to save settings: {}", e);
                    }
                }

                // Restart with the new backend.
                if let Err(e) = state.daemon.start().await {
                    error!("Failed to start daemon after backend switch: {}", e);
                    let dialog_app = app.clone();
                    let msg = format!("Failed to start backend: {}", e);
                    let _ = tokio::task::spawn_blocking(move || {
                        dialog_app
                            .dialog()
                            .message(msg)
                            .kind(MessageDialogKind::Error)
                            .title("Backend Switch Failed")
                            .blocking_show();
                    })
                    .await;
                } else {
                    update_status(&app, true);
                    info!("Switched backend to {}", new_backend);
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
            info!("Restarting ESPHome backend");
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
