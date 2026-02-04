//! ESPHome Builder Application
//!
//! A cross-platform desktop application that manages ESPHome as a background daemon
//! with system tray integration.

mod daemon;
mod platform;
mod settings;
mod tray;
mod update;

use anyhow::Result;
use std::sync::Arc;
use tauri::{
    async_runtime, AppHandle, Manager, RunEvent,
    tray::{MouseButton, MouseButtonState, TrayIconEvent},
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use daemon::DaemonManager;
use settings::Settings;
use tray::build_tray_menu;
use update::UpdateChecker;

/// Application state shared across the app
pub struct AppState {
    pub daemon: DaemonManager,
    pub settings: RwLock<Settings>,
    pub update_checker: UpdateChecker,
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
        })
    }
}

/// Open the ESPHome dashboard in the default browser
fn open_dashboard(port: u16) {
    let url = format!("http://localhost:{}", port);
    if let Err(e) = open::that(&url) {
        error!("Failed to open browser: {}", e);
    }
}

/// Wait for the dashboard to be ready by polling the health endpoint
async fn wait_for_dashboard_ready(port: u16, timeout_secs: u64) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let url = format!("http://localhost:{}/", port);
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                info!("Dashboard is ready");
                return true;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    warn!("Timeout waiting for dashboard to be ready");
    false
}

/// Initialize logging
fn init_logging() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "esphome_desktop=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// Handle tray icon left-click (open dashboard)
fn handle_tray_click(_app: &AppHandle, state: &AppState) {
    let settings = async_runtime::block_on(state.settings.read());
    open_dashboard(settings.port);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    init_logging();
    info!("Starting ESPHome Builder");

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
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
        .setup(|app| {
            info!("Setting up ESPHome Builder");

            // Ensure user venv exists (copy from bundled on first run)
            // This must happen before AppState::new() so paths are correct
            if let Err(e) = platform::ensure_user_venv(app.handle()) {
                error!("Failed to set up user venv: {}", e);
                // Continue anyway - might work with bundled venv
            }

            // Initialize app state
            let state = Arc::new(AppState::new(app.handle())?);
            app.manage(state.clone());

            // Build and set up the tray menu
            let menu = build_tray_menu(app.handle(), &state)?;

            // Get the tray icon handle and set up event handlers
            if let Some(tray) = app.tray_by_id("main") {
                tray.set_menu(Some(menu))?;
                tray.set_tooltip(Some("ESPHome Builder"))?;

                // Set up click handler
                let state_clone = state.clone();
                let app_handle = app.handle().clone();
                tray.on_tray_icon_event(move |_tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        handle_tray_click(&app_handle, &state_clone);
                    }
                });
            }

            // Start the daemon
            let daemon_state = state.clone();
            let daemon_app = app.handle().clone();
            async_runtime::spawn(async move {
                match daemon_state.daemon.start().await {
                    Ok(()) => {
                        // Update tray status to show running
                        tray::update_status(&daemon_app, true);
                    }
                    Err(e) => {
                        error!("Failed to start ESPHome daemon: {}", e);
                        tray::update_status(&daemon_app, false);
                    }
                }
            });

            // Start update checker (check once at startup, then periodically)
            let update_state = state.clone();
            let update_app = app.handle().clone();
            async_runtime::spawn(async move {
                // Wait a bit before first check
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                update_state
                    .update_checker
                    .check_and_notify(&update_app)
                    .await;

                // Check every 24 hours
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(86400));
                loop {
                    interval.tick().await;
                    update_state
                        .update_checker
                        .check_and_notify(&update_app)
                        .await;
                }
            });

            // Open dashboard on first start (after it's ready)
            let settings = async_runtime::block_on(state.settings.read());
            if settings.open_on_start {
                let port = settings.port;
                // Wait for dashboard to be ready, then open browser
                async_runtime::spawn(async move {
                    if wait_for_dashboard_ready(port, 60).await {
                        open_dashboard(port);
                    } else {
                        // Open anyway after timeout - user can refresh
                        open_dashboard(port);
                    }
                });
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let RunEvent::ExitRequested { .. } = event {
                // Stop the daemon before exiting
                if let Some(state) = app_handle.try_state::<Arc<AppState>>() {
                    info!("Stopping ESPHome daemon before exit");
                    async_runtime::block_on(async {
                        if let Err(e) = state.daemon.stop().await {
                            warn!("Error stopping daemon: {}", e);
                        }
                    });
                }
            }
        });
}
