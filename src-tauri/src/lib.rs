//! ESPHome Device Builder Application
//!
//! A cross-platform desktop application that manages ESPHome as a background daemon
//! with system tray integration.

mod app_update;
mod daemon;
mod git_check;
mod platform;
mod settings;
mod tray;
mod update;
mod util;

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
use settings::{Backend, Settings};
use tray::build_tray_menu;
use update::UpdateChecker;

/// CLI selector for the device-builder channel.
/// Maps onto [`Backend::BuilderStable`]/[`Backend::BuilderBeta`].
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "lowercase")]
pub enum BuilderChannelArg {
    Stable,
    Beta,
}

/// ESPHome Device Builder - System tray application for ESPHome
#[derive(Parser, Debug, Clone)]
#[command(name = "esphome-desktop")]
#[command(about = "ESPHome Device Builder", long_about = None)]
pub struct Cli {
    /// Don't open the dashboard in browser on startup
    #[arg(long = "no-open-dashboard")]
    pub no_open_dashboard: bool,

    /// Switch to the ESPHome Device Builder backend instead of the classic
    /// dashboard. Persists to settings — useful as a fallback when the tray
    /// menu is unavailable.
    #[arg(long = "use-builder")]
    pub use_builder: bool,

    /// Channel for the ESPHome Device Builder backend.
    /// Only takes effect together with `--use-builder`.
    #[arg(long = "builder-channel", value_enum, default_value_t = BuilderChannelArg::Beta)]
    pub builder_channel: BuilderChannelArg,
}

/// Application state shared across the app
pub struct AppState {
    pub daemon: DaemonManager,
    pub settings: RwLock<Settings>,
    pub update_checker: UpdateChecker,
    /// Guards the tray's multi-step stop→install→start sequences
    /// (Check for Updates / Switch Channel / Switch Backend) so only one
    /// runs at a time. Each of those menu arms spawns an independent async
    /// task; while `daemon.start()`/`stop()` are individually mutex-serialized,
    /// the *sequences* are not, so concurrent menu clicks could interleave at
    /// `await` points (e.g. one switch's `start()` racing another's mid-install)
    /// and stack confirmation dialogs. See `tray::UpdateGuard`.
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
/// is only reachable through the system tray menu. On Linux AppImage builds
/// running under desktops without a StatusNotifier host (e.g. some KDE Plasma
/// and GNOME setups) the tray icon never appears, so telling the user to
/// "open the tray menu" is misleading — there is no menu. In that case point
/// them at a path that actually works. See GitHub issue #87.
///
/// Today the no-tray branch is only reachable on Linux AppImage builds without
/// a StatusNotifier host, so the alternative wording is gated behind
/// `target_os = "linux"`. If a future code path ever sets `tray_available =
/// false` on macOS or Windows (e.g. a tray-init failure), those platforms get a
/// generic "reinstall the latest release" message instead of misleading
/// deb/rpm/AUR instructions.
pub(crate) fn updates_menu_hint(tray_available: bool) -> &'static str {
    if tray_available {
        "Open the tray menu and choose \"Check for Updates...\" to update."
    } else if cfg!(target_os = "linux") {
        "No system tray was detected, so the in-app updater is unavailable. \
         Install the deb/rpm/AUR package (which has a working tray) or reinstall \
         the latest release to update."
    } else {
        "No system tray was detected, so the in-app updater is unavailable. \
         Reinstall the latest release to update."
    }
}

/// Open the ESPHome dashboard in the default browser
fn open_dashboard(port: u16) {
    let url = format!("http://localhost:{}", port);
    if let Err(e) = open::that(&url) {
        error!("Failed to open browser: {}", e);
    }
}

/// Build the URL the startup readiness probe should poll.
///
/// The daemon is spawned with `--host 127.0.0.1` / `--address 127.0.0.1`
/// (see `DaemonManager::start()`), so it only listens on the IPv4 loopback.
/// We probe the literal `127.0.0.1` rather than the `localhost` hostname to
/// avoid a resolver detour: on IPv6-first hosts `localhost` resolves to
/// `::1` first, where nothing is listening, so each poll can stall on the
/// `::1` connect attempt before falling back to IPv4 — delaying the
/// browser-open on startup. The periodic health check should probe
/// `127.0.0.1` for the same reason.
fn dashboard_ready_url(port: u16) -> String {
    format!("http://127.0.0.1:{}/", port)
}

/// Wait for the dashboard to be ready by polling the health endpoint
pub(crate) async fn wait_for_dashboard_ready(port: u16, timeout_secs: u64) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };

    let url = dashboard_ready_url(port);
    let start = std::time::Instant::now();
    let timeout = std::time::Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        if let Ok(response) = client.get(&url).send().await {
            if response.status().is_success() {
                info!("Backend is ready");
                return true;
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
    }

    warn!("Timeout waiting for backend to be ready");
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
    info!("Starting ESPHome Device Builder");
    info!("CLI args: {:?}", cli);

    // Capture CLI flags before closure
    let no_open_dashboard = cli.no_open_dashboard;
    let cli_backend_override = if cli.use_builder {
        Some(match cli.builder_channel {
            BuilderChannelArg::Stable => Backend::BuilderStable,
            BuilderChannelArg::Beta => Backend::BuilderBeta,
        })
    } else {
        None
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
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
            info!("Setting up ESPHome Device Builder");

            // Ensure user Python exists (copy from bundled on first run for non-Windows)
            // This must happen before AppState::new() so paths are correct
            if let Err(e) = platform::ensure_user_python(app.handle()) {
                error!("Failed to set up user Python: {}", e);
                // Continue anyway - might work with bundled Python
            }

            // One-shot prompt to remove the pre-rename `/Applications/ESPHome Builder.app`.
            // No-op on non-macOS and after the user has answered once.
            platform::cleanup_legacy_macos_app(app.handle());

            // Initialize app state
            let state = Arc::new(AppState::new(app.handle())?);
            app.manage(state.clone());

            // Apply CLI backend override (persists to settings).
            // This runs before the daemon starts so the new backend takes
            // effect immediately, and before the tray menu is built so the
            // radio buttons reflect the override.
            let cli_override_needs_install = if let Some(new_backend) = cli_backend_override {
                let mut settings = async_runtime::block_on(state.settings.write());
                if settings.backend != new_backend {
                    info!(
                        "CLI override: switching backend from {} to {}",
                        settings.backend, new_backend
                    );
                    settings.backend = new_backend;
                    if let Err(e) = settings.save(app.handle()) {
                        warn!("Failed to save settings after CLI override: {}", e);
                    }
                    state
                        .daemon
                        .set_use_device_builder(new_backend.is_builder());
                    new_backend.is_builder()
                } else {
                    false
                }
            } else {
                false
            };

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
                        .tooltip("ESPHome Device Builder")
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
                        warn!(
                            "Failed to create system tray icon: {}. Running without tray.",
                            e
                        );
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
                // If a CLI override switched us into a builder backend, ensure
                // the package is installed/upgraded before starting the daemon.
                if cli_override_needs_install {
                    let backend = daemon_state.settings.read().await.backend;
                    info!("Installing/upgrading esphome-device-builder for CLI override");
                    if let Err(e) = daemon_state
                        .update_checker
                        .install_device_builder(&daemon_app, backend)
                        .await
                    {
                        error!("Failed to install esphome-device-builder: {}", e);
                    }
                }

                match daemon_state.daemon.start().await {
                    Ok(()) => {
                        // Update tray status to show running
                        tray::update_status(&daemon_app, true);

                        // Warn (non-blocking) if git is missing. ESPHome needs
                        // it for external components, remote packages, and other
                        // deps, so many configs won't compile without it; absent
                        // git they fail with a cryptic Python traceback instead
                        // of a clear message. Only after a successful start, so
                        // we don't stack a git warning onto an unrelated startup
                        // failure.
                        git_check::notify_if_git_missing(&daemon_app);
                    }
                    Err(e) => {
                        error!("Failed to start ESPHome daemon: {}", e);
                        tray::update_status(&daemon_app, false);
                    }
                }
            });

            // Start update checker (check after 30s, then every 24 hours)
            // Order matters: check the desktop app first. A self-update ships
            // a fresh Python bundle that overwrites the user's `python/`
            // directory, so any pip-installed ESPHome / device-builder bump
            // we'd do now would be wiped by the next launch. Skip the Python
            // checks while an app update is pending.
            // The dev channel skips automatic update checks entirely. When
            // the active backend is a builder variant, the
            // `esphome-device-builder` package is checked on the same schedule.
            let update_state = state.clone();
            let update_app = app.handle().clone();
            // Captured so background update notifications can adapt their
            // "how to update" hint when there is no tray menu to point at
            // (issue #87).
            let update_tray_available = tray_available;
            async_runtime::spawn(async move {
                tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(86400));
                loop {
                    interval.tick().await;
                    if app_update::check_and_notify(&update_app, update_tray_available).await
                        == app_update::NextStep::Skip
                    {
                        // App update pending — leave the Python packages alone.
                        continue;
                    }
                    let (channel, backend) = {
                        let settings = update_state.settings.read().await;
                        (settings.release_channel, settings.backend)
                    };
                    update_state
                        .update_checker
                        .check_and_notify(&update_app, channel, update_tray_available)
                        .await;
                    if backend.is_builder() {
                        update_state
                            .update_checker
                            .check_and_notify_device_builder(
                                &update_app,
                                backend,
                                update_tray_available,
                            )
                            .await;
                    }
                }
            });

            // Set up signal handlers for graceful shutdown on Ctrl+C.
            // The daemon-stop is handled by the RunEvent::ExitRequested
            // branch in run() below; we just trip the exit here.
            #[cfg(unix)]
            {
                let signal_app = app.handle().clone();
                async_runtime::spawn(async move {
                    let mut sigint =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                            .expect("Failed to set up SIGINT handler");
                    let mut sigterm =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("Failed to set up SIGTERM handler");

                    tokio::select! {
                        _ = sigint.recv() => {
                            info!("Received SIGINT, shutting down...");
                        }
                        _ = sigterm.recv() => {
                            info!("Received SIGTERM, shutting down...");
                        }
                    }

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
                info!("Opening backend in browser on startup");
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
                info!("Browser opening suppressed by --no-open-dashboard flag");
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            // Synchronously SIGTERM the dashboard's process group on any
            // exit-related event so the signal is in the kernel before
            // we attempt anything else. Covers two scenarios:
            //
            // * macOS Dock right-click → Quit, which on this Tauri
            //   version only fires `RunEvent::Exit` (not ExitRequested)
            //   after the runtime is already winding down.
            // * A future Tauri version that DOES fire ExitRequested for
            //   Dock-Quit but doesn't honor `prevent_exit()` long enough
            //   for the spawned graceful-stop task below to actually
            //   run.
            //
            // `terminate_blocking` is idempotent (atomic-swap on the
            // stored PID), doesn't touch the `running` flag, and is
            // a no-op once `stop()` has already cleared the PID — so
            // calling it on both events is safe and double-firing the
            // SIGTERM is harmless (the kernel coalesces).
            if matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. }) {
                if let Some(state) = app_handle.try_state::<Arc<AppState>>() {
                    state.daemon.terminate_blocking();
                }
            }

            if let RunEvent::ExitRequested { api, .. } = event {
                // Running daemon.stop() via async_runtime::block_on from
                // inside the run() callback orphans the dashboard child:
                // the future does not actually run to completion before
                // Tauri tears the process down, so SIGTERM is never sent.
                // Prevent the immediate exit, drain the daemon on the
                // tokio runtime via spawn (the same shape the SIGINT/
                // SIGTERM handler uses), then call app.exit(0) which
                // re-enters this branch with running=false and falls
                // through to a clean exit.
                if let Some(state) = app_handle.try_state::<Arc<AppState>>() {
                    if state.daemon.is_running() {
                        api.prevent_exit();
                        let state_clone: Arc<AppState> = state.inner().clone();
                        let app = app_handle.clone();
                        async_runtime::spawn(async move {
                            info!("Stopping ESPHome daemon before exit");
                            if let Err(e) = state_clone.daemon.stop().await {
                                warn!("Error stopping daemon: {}", e);
                            }
                            app.exit(0);
                        });
                    }
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::dashboard_ready_url;
    use super::updates_menu_hint;

    #[test]
    fn dashboard_ready_url_targets_ipv4_loopback() {
        // The daemon binds `127.0.0.1` only, so the readiness probe must
        // target the IPv4 literal rather than the `localhost` hostname —
        // otherwise IPv6-first hosts steer the connect to `::1`, where
        // nothing is listening, stalling each poll before the IPv4 fallback.
        let url = dashboard_ready_url(6052);
        assert_eq!(url, "http://127.0.0.1:6052/");
        assert!(!url.contains("localhost"));
    }

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
        // Must offer a concrete alternative.
        assert!(hint.contains("deb/rpm/AUR") || hint.contains("reinstall"));
    }
}
