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
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_dialog::MessageDialogKind;
use tauri_plugin_notification::NotificationExt;
use tracing::{error, info, warn};

use crate::control::ops::{self, SwitchOutcome, UpdateGuard};
use crate::i18n::{t, t_with};
use crate::settings::{Backend, ReleaseChannel};
use crate::AppState;

/// Menu item IDs
mod ids {
    pub const OPEN_DASHBOARD: &str = "open_dashboard";
    pub const STATUS: &str = "status";
    pub const APP_VERSION: &str = "app_version";
    pub const VERSION: &str = "version";
    pub const BUILDER_VERSION: &str = "builder_version";
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
    pub const BACKEND_BUILDER_STABLE: &str = "backend_builder_stable";
    pub const BACKEND_BUILDER_BETA: &str = "backend_builder_beta";

    // Startup submenu items
    pub const STARTUP_ENABLE: &str = "startup_enable";
    pub const STARTUP_DISABLE: &str = "startup_disable";
}

/// Build the tray menu
pub fn build_tray_menu(app_handle: &AppHandle, state: &Arc<AppState>) -> Result<Menu<tauri::Wry>> {
    let settings = async_runtime::block_on(state.settings.read());
    let status_text = if state.daemon.is_running() {
        t("tray.status_running")
    } else {
        t("tray.status_starting")
    };

    // Create status item and store it for later updates
    let status_item = MenuItemBuilder::with_id(ids::STATUS, status_text)
        .enabled(false)
        .build(app_handle)?;
    let _ = STATUS_ITEM.set(status_item.clone());

    // Create desktop app version display item (Tauri app version from
    // tauri.conf.json — fixed for the lifetime of the process, never updated).
    let app_version_text = t_with(
        "tray.desktop_version",
        &[("version", &app_handle.package_info().version.to_string())],
    );
    let app_version_item = MenuItemBuilder::with_id(ids::APP_VERSION, app_version_text)
        .enabled(false)
        .build(app_handle)?;

    // Create ESPHome version display item
    let version_text = t_with(
        "tray.esphome_version",
        &[(
            "version",
            settings
                .installed_version
                .as_deref()
                .unwrap_or(&t("version.unknown")),
        )],
    );
    let version_item = MenuItemBuilder::with_id(ids::VERSION, version_text)
        .enabled(false)
        .build(app_handle)?;
    let _ = VERSION_ITEM.set(version_item.clone());

    // Create esphome-device-builder version display item. Always shown so the
    // menu structure is stable. Detection spawns a Python subprocess which is
    // too slow / too risky (could hang) to run synchronously in the setup
    // path, so we start with a "detecting…" placeholder and refresh from a
    // background task immediately below.
    let builder_version_item = MenuItemBuilder::with_id(
        ids::BUILDER_VERSION,
        t_with(
            "tray.builder_version",
            &[("version", &t("version.detecting"))],
        ),
    )
    .enabled(false)
    .build(app_handle)?;
    let _ = BUILDER_VERSION_ITEM.set(builder_version_item.clone());

    // Kick off async detection of the installed `esphome-device-builder`
    // version. The blocking Python call runs on a dedicated thread so it
    // can't stall tray creation or other setup work.
    {
        let app = app_handle.clone();
        async_runtime::spawn(async move {
            refresh_builder_version_display(&app).await;
        });
    }

    // Create release channel items
    let current_channel = settings.release_channel;
    let channel_stable = CHANNEL_STABLE_ITEM.build(
        app_handle,
        ids::CHANNEL_STABLE,
        current_channel == ReleaseChannel::Stable,
    )?;
    let channel_beta = CHANNEL_BETA_ITEM.build(
        app_handle,
        ids::CHANNEL_BETA,
        current_channel == ReleaseChannel::Beta,
    )?;
    let channel_dev = CHANNEL_DEV_ITEM.build(
        app_handle,
        ids::CHANNEL_DEV,
        current_channel == ReleaseChannel::Dev,
    )?;

    let channel_submenu =
        SubmenuBuilder::with_id(app_handle, "release_channel", t("tray.release_channel"))
            .item(&channel_stable)
            .item(&channel_beta)
            .item(&channel_dev)
            .build()?;

    // Backend submenu items
    let current_backend = settings.backend;
    // TODO: use `build` once a stable release of esphome-device-builder is out
    let backend_builder_stable = BACKEND_BUILDER_STABLE_ITEM.build_disabled(
        app_handle,
        ids::BACKEND_BUILDER_STABLE,
        current_backend == Backend::BuilderStable,
    )?;
    let backend_builder_beta = BACKEND_BUILDER_BETA_ITEM.build(
        app_handle,
        ids::BACKEND_BUILDER_BETA,
        current_backend == Backend::BuilderBeta,
    )?;

    let backend_submenu = SubmenuBuilder::with_id(app_handle, "backend", t("tray.backend"))
        .item(&backend_builder_stable)
        .item(&backend_builder_beta)
        .build()?;

    // Startup submenu items (radio group, mirroring Backend). Label from the
    // actual OS login-item state so a failed startup reconcile doesn't show a
    // lie; fall back to the persisted intent only if the query itself errors.
    let launch_at_startup = app_handle
        .autolaunch()
        .is_enabled()
        .unwrap_or(settings.launch_at_startup);
    let startup_enable =
        STARTUP_ENABLE_ITEM.build(app_handle, ids::STARTUP_ENABLE, launch_at_startup)?;
    let startup_disable =
        STARTUP_DISABLE_ITEM.build(app_handle, ids::STARTUP_DISABLE, !launch_at_startup)?;

    let startup_submenu = SubmenuBuilder::with_id(app_handle, "startup", t("tray.startup"))
        .item(&startup_enable)
        .item(&startup_disable)
        .build()?;

    let menu = MenuBuilder::new(app_handle)
        .item(
            &MenuItemBuilder::with_id(ids::OPEN_DASHBOARD, t("tray.open_dashboard"))
                .accelerator("CmdOrCtrl+O")
                .build(app_handle)?,
        )
        .separator()
        .item(&status_item)
        .item(&app_version_item)
        .item(&version_item)
        .item(&builder_version_item)
        .item(
            &MenuItemBuilder::with_id(
                ids::PORT,
                t_with("tray.port", &[("port", &settings.port.to_string())]),
            )
            .enabled(false)
            .build(app_handle)?,
        )
        .separator()
        .item(&backend_submenu)
        .item(&channel_submenu)
        .item(&startup_submenu)
        .item(
            &MenuItemBuilder::with_id(ids::CHECK_UPDATES, t("tray.check_updates"))
                .build(app_handle)?,
        )
        .separator()
        .item(&MenuItemBuilder::with_id(ids::VIEW_LOGS, t("tray.view_logs")).build(app_handle)?)
        .item(&MenuItemBuilder::with_id(ids::OPEN_CONFIG, t("tray.open_config")).build(app_handle)?)
        .item(
            &MenuItemBuilder::with_id(ids::RESTART, t("tray.restart_dashboard"))
                .build(app_handle)?,
        )
        .separator()
        .item(&MenuItemBuilder::with_id(ids::QUIT, t("tray.quit")).build(app_handle)?)
        .build()?;

    // Set up menu event handler
    let state_clone = state.clone();
    app_handle.on_menu_event(move |app_handle, event| {
        handle_menu_event(app_handle, event.id().as_ref(), &state_clone);
    });

    Ok(menu)
}

/// Status menu item stored globally for updates
static STATUS_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> = std::sync::OnceLock::new();

/// Version menu item stored globally for updates
static VERSION_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> = std::sync::OnceLock::new();

/// `esphome-device-builder` version menu item stored globally for updates
static BUILDER_VERSION_ITEM: std::sync::OnceLock<MenuItem<tauri::Wry>> = std::sync::OnceLock::new();

/// A radio-style menu entry: the base label plus the globally stored menu
/// item. `build` creates the item and registers it; `refresh` rewrites its
/// label to reflect the current selection.
///
/// The label is a function rather than a string so translated labels resolve
/// through `i18n::t` at build/refresh time; untranslated product terms
/// (channel and backend names) just return their literal.
struct RadioItem {
    label: fn() -> String,
    item: std::sync::OnceLock<MenuItem<tauri::Wry>>,
}

impl RadioItem {
    const fn new(label: fn() -> String) -> Self {
        Self {
            label,
            item: std::sync::OnceLock::new(),
        }
    }

    /// Build the menu item with a `radio_label` and register it for later
    /// refreshes.
    fn build(
        &self,
        app_handle: &AppHandle,
        id: &str,
        selected: bool,
    ) -> Result<MenuItem<tauri::Wry>> {
        self.build_impl(app_handle, id, selected, true)
    }

    /// Like [`RadioItem::build`], but the item is greyed out.
    fn build_disabled(
        &self,
        app_handle: &AppHandle,
        id: &str,
        selected: bool,
    ) -> Result<MenuItem<tauri::Wry>> {
        self.build_impl(app_handle, id, selected, false)
    }

    fn build_impl(
        &self,
        app_handle: &AppHandle,
        id: &str,
        selected: bool,
        enabled: bool,
    ) -> Result<MenuItem<tauri::Wry>> {
        // If a previous build already registered an item, reuse it so
        // `refresh` keeps targeting the item that is live in the menu.
        if let Some(item) = self.item.get() {
            item.set_text(radio_label(&(self.label)(), selected))?;
            item.set_enabled(enabled)?;
            return Ok(item.clone());
        }
        let item = MenuItemBuilder::with_id(id, radio_label(&(self.label)(), selected))
            .enabled(enabled)
            .build(app_handle)?;
        let _ = self.item.set(item.clone());
        Ok(item)
    }

    /// Update the registered item's label to reflect the current selection.
    fn refresh(&self, selected: bool) {
        if let Some(item) = self.item.get() {
            let label = (self.label)();
            if let Err(e) = item.set_text(radio_label(&label, selected)) {
                warn!("Failed to update tray menu item '{}': {}", label, e);
            }
        }
    }
}

/// Release channel items stored globally for radio-button behavior.
/// Channel names are deliberately untranslated (product terms).
static CHANNEL_STABLE_ITEM: RadioItem = RadioItem::new(|| "Stable".to_string());
static CHANNEL_BETA_ITEM: RadioItem = RadioItem::new(|| "Beta".to_string());
static CHANNEL_DEV_ITEM: RadioItem = RadioItem::new(|| "Dev".to_string());

/// Backend menu items stored globally for radio-button behavior.
/// Backend names are deliberately untranslated (product terms).
static BACKEND_BUILDER_STABLE_ITEM: RadioItem =
    RadioItem::new(|| "ESPHome Device Builder (stable)".to_string());
static BACKEND_BUILDER_BETA_ITEM: RadioItem =
    RadioItem::new(|| "ESPHome Device Builder (beta)".to_string());

/// Startup menu items stored globally for radio-button behavior
static STARTUP_ENABLE_ITEM: RadioItem = RadioItem::new(|| t("tray.launch_at_login"));
static STARTUP_DISABLE_ITEM: RadioItem = RadioItem::new(|| t("tray.dont_launch_at_login"));

/// Update the tray status text
pub fn update_status(_app_handle: &AppHandle, running: bool) {
    let status_text = if running {
        t("tray.status_running")
    } else {
        t("tray.status_stopped")
    };

    if let Some(item) = STATUS_ITEM.get() {
        let _ = item.set_text(status_text);
    }
}

/// Update the version display in the tray menu.
pub fn update_version(version: &str) {
    if let Some(item) = VERSION_ITEM.get() {
        let _ = item.set_text(t_with("tray.esphome_version", &[("version", version)]));
    }
}

/// Update the `esphome-device-builder` version display in the tray menu.
pub fn update_builder_version(version: &str) {
    if let Some(item) = BUILDER_VERSION_ITEM.get() {
        let _ = item.set_text(t_with("tray.builder_version", &[("version", version)]));
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
pub(crate) fn update_channel_checks(channel: ReleaseChannel) {
    CHANNEL_STABLE_ITEM.refresh(channel == ReleaseChannel::Stable);
    CHANNEL_BETA_ITEM.refresh(channel == ReleaseChannel::Beta);
    CHANNEL_DEV_ITEM.refresh(channel == ReleaseChannel::Dev);
}

/// Update the backend menu item labels to reflect the given backend.
pub(crate) fn update_backend_checks(backend: Backend) {
    BACKEND_BUILDER_STABLE_ITEM.refresh(backend == Backend::BuilderStable);
    BACKEND_BUILDER_BETA_ITEM.refresh(backend == Backend::BuilderBeta);
}

/// Update the startup menu item labels to reflect whether autostart is enabled.
pub(crate) fn update_startup_checks(enabled: bool) {
    STARTUP_ENABLE_ITEM.refresh(enabled);
    STARTUP_DISABLE_ITEM.refresh(!enabled);
}

/// Re-detect the installed version and update the tray version display.
pub(crate) fn refresh_version_display(app_handle: &AppHandle) {
    // Mirror the device-builder display: keep "not installed" distinct from a
    // real detection failure ("unknown") instead of collapsing both.
    let version = match crate::update::installed_esphome_version(app_handle) {
        Ok(Some(v)) => v,
        Ok(None) => t("version.not_installed"),
        Err(e) => {
            warn!("Could not detect ESPHome version: {}", e);
            t("version.unknown")
        }
    };
    update_version(&version);
}

/// Re-detect the installed `esphome-device-builder` package version and
/// update the tray display. Runs the blocking Python call off the caller's
/// thread, and distinguishes "package not installed" from "detection
/// failed" so the latter doesn't get silently misreported.
pub(crate) async fn refresh_builder_version_display(app_handle: &AppHandle) {
    let app = app_handle.clone();
    let label = tokio::task::spawn_blocking(move || {
        match crate::update::get_installed_device_builder_version(&app) {
            Ok(Some(v)) => v,
            Ok(None) => t("version.not_installed"),
            Err(e) => {
                warn!("Could not detect esphome-device-builder version: {}", e);
                t("version.unknown")
            }
        }
    })
    .await
    .unwrap_or_else(|e| {
        warn!("Device-builder version detection task failed: {}", e);
        t("version.unknown")
    });
    update_builder_version(&label);
}

/// Handle menu item clicks
fn handle_menu_event(app_handle: &AppHandle, id: &str, state: &Arc<AppState>) {
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

                            // Update the version display in the tray menu
                            refresh_version_display(&app);

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
