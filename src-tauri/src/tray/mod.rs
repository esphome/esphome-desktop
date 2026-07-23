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
use tracing::warn;

use crate::i18n::{t, t_with};
use crate::settings::{Backend, ReleaseChannel};
use crate::AppState;

mod events;

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
    let backend_builder_stable = BACKEND_BUILDER_STABLE_ITEM.build(
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
        events::handle_menu_event(app_handle, event.id().as_ref(), &state_clone);
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
    #[allow(dead_code)]
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
