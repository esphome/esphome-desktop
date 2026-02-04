//! System tray menu
//!
//! Handles the system tray icon and context menu.

use anyhow::Result;
use std::sync::Arc;
use tauri::{
    async_runtime,
    menu::{Menu, MenuBuilder, MenuItem, MenuItemBuilder},
    AppHandle,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tracing::{error, info};

use crate::AppState;

/// Menu item IDs
mod ids {
    pub const OPEN_DASHBOARD: &str = "open_dashboard";
    pub const STATUS: &str = "status";
    pub const PORT: &str = "port";
    pub const CHECK_UPDATES: &str = "check_updates";
    pub const VIEW_LOGS: &str = "view_logs";
    pub const OPEN_CONFIG: &str = "open_config";
    pub const RESTART: &str = "restart";
    pub const QUIT: &str = "quit";
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

    let menu = MenuBuilder::new(app_handle)
        .item(
            &MenuItemBuilder::with_id(ids::OPEN_DASHBOARD, "Open Dashboard")
                .accelerator("CmdOrCtrl+O")
                .build(app_handle)?,
        )
        .separator()
        .item(&status_item)
        .item(
            &MenuItemBuilder::with_id(ids::PORT, format!("Port: {}", settings.port))
                .enabled(false)
                .build(app_handle)?,
        )
        .separator()
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
                // Check for updates and get version if user wants to update
                if let Some(version) = state.update_checker.check_for_user(&app).await {
                    info!("User requested update to version {}", version);

                    // Stop the dashboard
                    update_status(&app, false);
                    if let Err(e) = state.daemon.stop().await {
                        error!("Failed to stop dashboard for update: {}", e);
                        app.dialog()
                            .message(format!("Failed to stop dashboard: {}", e))
                            .kind(MessageDialogKind::Error)
                            .title("Update Failed")
                            .blocking_show();
                        return;
                    }

                    // Perform the update
                    match state.update_checker.update_to(&app, &version).await {
                        Ok(()) => {
                            info!("Update completed successfully");

                            // Restart the dashboard
                            if let Err(e) = state.daemon.start().await {
                                error!("Failed to restart dashboard after update: {}", e);
                                app.dialog()
                                    .message(format!(
                                        "ESPHome updated to {}, but failed to restart dashboard: {}",
                                        version, e
                                    ))
                                    .kind(MessageDialogKind::Warning)
                                    .title("Update Partially Complete")
                                    .blocking_show();
                            } else {
                                update_status(&app, true);
                                app.dialog()
                                    .message(format!(
                                        "ESPHome has been updated to version {}.",
                                        version
                                    ))
                                    .kind(MessageDialogKind::Info)
                                    .title("Update Complete")
                                    .blocking_show();
                            }
                        }
                        Err(e) => {
                            error!("Update failed: {}", e);
                            app.dialog()
                                .message(format!("Failed to update ESPHome: {}", e))
                                .kind(MessageDialogKind::Error)
                                .title("Update Failed")
                                .blocking_show();

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
