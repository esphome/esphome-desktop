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
use clap::Parser;
use std::sync::Arc;
use tauri::{
    async_runtime,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager, RunEvent,
};
use tokio::sync::RwLock;
use tracing::{error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use daemon::DaemonManager;
use settings::Settings;
use tray::build_tray_menu;
use update::UpdateChecker;

/// ESPHome Desktop - System tray application for ESPHome
#[derive(Parser, Debug, Clone)]
#[command(name = "esphome-desktop")]
#[command(about = "ESPHome Desktop Builder", long_about = None)]
pub struct Cli {
    /// Don't open the dashboard in browser on startup
    #[arg(long = "no-open-dashboard")]
    pub no_open_dashboard: bool,
}

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
pub fn run(cli: Cli) {
    init_logging();
    info!("Starting ESPHome Builder");
    info!("CLI args: {:?}", cli);

    // Capture CLI flags before closure
    let no_open_dashboard = cli.no_open_dashboard;

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
        .setup(move |app| {
            info!("Setting up ESPHome Builder");

            // Ensure user Python exists (copy from bundled on first run for non-Windows)
            // This must happen before AppState::new() so paths are correct
            if let Err(e) = platform::ensure_user_python(app.handle()) {
                error!("Failed to set up user Python: {}", e);
                // Continue anyway - might work with bundled Python
            }

            // Initialize app state
            let state = Arc::new(AppState::new(app.handle())?);
            app.manage(state.clone());

            // Build and set up the tray menu (if tray support is available)
            let tray_available = if platform::is_tray_supported() {
                // Create the tray icon programmatically.
                // We wrap this in catch_unwind as a safety net: on Linux the
                // underlying libappindicator-sys crate will panic!() if the
                // shared library fails to load (e.g. GLIBC version mismatch).
                let tray_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let icon = app
                        .default_window_icon()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("No default icon available for tray"))?;

                    let tray = TrayIconBuilder::with_id("main")
                        .icon(icon)
                        .icon_as_template(false)
                        .tooltip("ESPHome Builder")
                        .build(app)?;

                    let menu = build_tray_menu(app.handle(), &state)?;
                    tray.set_menu(Some(menu))?;

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

                    Ok::<(), anyhow::Error>(())
                }));

                match tray_result {
                    Ok(Ok(())) => {
                        info!("System tray icon created successfully");
                        true
                    }
                    Ok(Err(e)) => {
                        warn!("Failed to create system tray icon: {}. Running without tray.", e);
                        false
                    }
                    Err(_) => {
                        warn!(
                            "System tray creation panicked (appindicator library not usable?). \
                             Running without tray."
                        );
                        false
                    }
                }
            } else {
                warn!(
                    "System tray not supported (appindicator library not found). \
                     Running without tray."
                );
                false
            };

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

            // Start update checker (check after 30s, then every 24 hours)
            let update_state = state.clone();
            let update_app = app.handle().clone();
            async_runtime::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(86400));
                loop {
                    interval.tick().await;
                    update_state
                        .update_checker
                        .check_and_notify(&update_app)
                        .await;
                }
            });

            // Set up signal handlers for graceful shutdown on Ctrl+C
            #[cfg(unix)]
            {
                let signal_state = state.clone();
                let signal_app = app.handle().clone();
                async_runtime::spawn(async move {
                    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                        .expect("Failed to set up SIGINT handler");
                    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("Failed to set up SIGTERM handler");

                    tokio::select! {
                        _ = sigint.recv() => {
                            info!("Received SIGINT, shutting down...");
                        }
                        _ = sigterm.recv() => {
                            info!("Received SIGTERM, shutting down...");
                        }
                    }

                    // Stop the daemon
                    if let Err(e) = signal_state.daemon.stop().await {
                        error!("Error stopping daemon on signal: {}", e);
                    }

                    // Exit the app
                    signal_app.exit(0);
                });
            }

            // Open dashboard on first start (after it's ready)
            let settings = async_runtime::block_on(state.settings.read());
            // Always open the dashboard if there's no tray (the user needs some
            // way to interact with the app), unless explicitly suppressed.
            let should_open = (settings.open_on_start || !tray_available) && !no_open_dashboard;
            if should_open {
                let port = settings.port;
                info!("Opening dashboard on startup");
                // Wait for dashboard to be ready, then open browser
                async_runtime::spawn(async move {
                    if wait_for_dashboard_ready(port, 60).await {
                        open_dashboard(port);
                    } else {
                        // Open anyway after timeout - user can refresh
                        open_dashboard(port);
                    }
                });
            } else if no_open_dashboard {
                info!("Dashboard opening suppressed by --no-open-dashboard flag");
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
