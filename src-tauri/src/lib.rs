//! ESPHome Device Builder Application
//!
//! A cross-platform desktop application that manages ESPHome as a background daemon
//! with system tray integration.

mod app_update;
mod cli;
mod control;
mod daemon;
mod dashboard;
mod dialog;
mod git_check;
mod i18n;
mod logging;
mod platform;
mod settings;
mod shutdown;
mod startup;
mod tray;
mod update;
mod util;

// The CLI argument model and pre-parse launch helpers live in `cli`; re-export
// them so `esphome_desktop_lib::Cli`, `crate::CliCommand`, etc. resolve as
// before.
pub use cli::*;

// The dashboard hand-off helpers live in `dashboard`; re-export them at the
// crate root, which is how every caller already reaches them.
pub(crate) use dashboard::{open_dashboard, wait_for_dashboard_ready};

use anyhow::Result;
use std::sync::Arc;
use tauri::{async_runtime, AppHandle, Manager};
use tauri_plugin_autostart::MacosLauncher;
use tokio::sync::RwLock;
use tracing::info;

use daemon::DaemonManager;
use settings::{Backend, Settings};
use update::UpdateChecker;

/// Application state shared across the app
pub struct AppState {
    pub daemon: DaemonManager,
    pub settings: RwLock<Settings>,
    pub update_checker: UpdateChecker,
    /// Guards the multi-step stop→install→start sequences — the tray's
    /// Check for Updates / Switch Channel / Switch Backend arms, their CLI
    /// counterparts, and the initial daemon start — so only one runs at a
    /// time. Each runs as an independent async task; while
    /// `daemon.start()`/`stop()` are individually mutex-serialized, the
    /// *sequences* are not, so concurrent triggers could interleave at
    /// `await` points (e.g. one switch's `start()` racing another's
    /// mid-install). See `control::ops::UpdateGuard`.
    pub update_in_flight: Arc<std::sync::atomic::AtomicBool>,
}

impl AppState {
    pub fn new(app_handle: &AppHandle) -> Result<Self> {
        let settings = Settings::load(app_handle)?;
        let daemon = DaemonManager::new(app_handle, &settings)?;
        let update_checker = UpdateChecker::new();

        Ok(Self {
            daemon,
            settings: RwLock::new(settings),
            update_checker,
            update_in_flight: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}

/// Build the "how to update" hint appended to background update notifications.
///
/// The in-app updater (for the desktop app, ESPHome, and the device builder)
/// is normally reached through the system tray menu. On Linux AppImage builds
/// running under desktops without a StatusNotifier host (e.g. some KDE Plasma
/// and GNOME setups) the tray icon never appears, so telling the user to
/// "open the tray menu" is misleading — there is no menu. Point them at the
/// CLI instead, which drives the same update flow over the control channel.
/// See GitHub issue #87.
pub(crate) fn updates_menu_hint(tray_available: bool) -> String {
    if tray_available {
        i18n::t("hint.updates_menu")
    } else {
        i18n::t("hint.updates_cli")
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run(cli: Cli) {
    logging::init_logging();
    info!("Starting ESPHome Device Builder");
    info!("CLI args: {:?}", cli);

    // Capture CLI flags before closure
    let no_open_dashboard = cli.no_open_dashboard;
    let cli_backend_override = if cli.use_builder {
        Some(Backend::from(cli.builder_channel))
    } else {
        None
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Launch silently on login (tray only, no browser) so a remote builder
        // comes back online after a reboot; manual launches still open the
        // dashboard. Whether the login item is registered is reconciled to the
        // `launch_at_startup` setting in setup() below.
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--no-open-dashboard"]),
        ))
        .plugin(tauri_plugin_single_instance::init(|app, args, cwd| {
            // Another instance tried to start - open the dashboard instead
            info!(
                "Single instance triggered from {:?} with args {:?}",
                cwd, args
            );
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                let settings = async_runtime::block_on(state.settings.read());
                open_dashboard(settings.port);
            }
        }))
        .setup(move |app| startup::setup(app, cli_backend_override, no_open_dashboard))
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(shutdown::on_run_event);
}

#[cfg(test)]
mod tests {
    use super::updates_menu_hint;

    #[test]
    fn hint_points_to_tray_when_available() {
        let hint = updates_menu_hint(true);
        assert!(hint.contains("tray menu"));
        assert!(hint.contains("Check for Updates"));
    }

    #[test]
    fn hint_avoids_tray_instructions_when_unavailable() {
        let hint = updates_menu_hint(false);
        // Must not tell the user to use a tray menu that isn't there (issue #87).
        assert!(!hint.contains("tray menu"));
        // Must offer a concrete alternative: the CLI update command.
        assert!(hint.contains("esphome-desktop update"));
    }
}
