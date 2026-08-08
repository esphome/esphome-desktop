//! The background tasks [`super::setup`] spawns once the app is wired up.
//!
//! Each is fire-and-forget on the tokio runtime: none of them may block the
//! setup hook, because Tauri does not show the tray or accept events until
//! it returns.

use std::sync::Arc;
use tauri::{async_runtime, AppHandle};
use tracing::{error, info};

use crate::{app_update, control, git_check, open_dashboard, wait_for_dashboard_ready, AppState};

/// Install the CLI-override package if needed, then start the daemon.
pub(super) fn spawn_daemon_start(
    app_handle: &AppHandle,
    state: &Arc<AppState>,
    cli_override_needs_install: bool,
) {
    let daemon_state = Arc::clone(state);
    let daemon_app = app_handle.clone();
    async_runtime::spawn(async move {
        // The control server is already accepting requests, so an
        // early CLI `update`/`release-channel` could otherwise
        // interleave its stop→install→start with this initial
        // install/start (e.g. its stop() no-ops before our start()
        // spawns the old backend mid-install). Hold the same guard
        // the update/switch sequences use; at startup it is almost
        // always free, so this settles immediately.
        let startup_guard =
            control::ops::UpdateGuard::acquire_wait(daemon_state.update_in_flight.clone()).await;

        // Repair a Python tree that a previous `--ignore-installed`
        // fallback left with orphaned files, which breaks every compile
        // (#330). Runs here rather than in `setup()` so it can await the
        // reinstall without blocking the launch, and inside the guard,
        // before the daemon exists: nothing is holding the packages open
        // yet, and a broken tree has nothing worth serving.
        daemon_state
            .update_checker
            .repair_python_tree_if_broken(&daemon_app)
            .await;

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

        let start_result = daemon_state.daemon.start().await;
        drop(startup_guard);
        match start_result {
            Ok(()) => {
                // Warn (non-blocking) if git is missing. ESPHome needs
                // it for external components, remote packages, and other
                // deps, so many configs won't compile without it; absent
                // git they fail with a cryptic Python traceback instead
                // of a clear message. Only after a successful start, so
                // we don't stack a git warning onto an unrelated startup
                // failure.
                git_check::notify_if_git_missing(&daemon_app);

                // Warn (non-blocking) if the config directory lives
                // inside an unrelated Git repository. ESP-IDF's CMake
                // git-revision detection walks upward and picks up the
                // stray repo, failing the build with an opaque
                // "head-ref" error rather than anything actionable
                // (issue #170).
                git_check::notify_if_config_dir_in_git_repo(
                    &daemon_app,
                    daemon_state.daemon.config_dir(),
                );
            }
            Err(e) => {
                error!("Failed to start ESPHome daemon: {}", e);
            }
        }
    });
}

/// Poll for desktop-app, ESPHome, and device-builder updates on a daily loop.
pub(super) fn spawn_update_checker(
    app_handle: &AppHandle,
    state: &Arc<AppState>,
    tray_available: bool,
) {
    // Order matters: check the desktop app first. A self-update ships
    // a fresh Python bundle that overwrites the user's `python/`
    // directory, so any pip-installed ESPHome / device-builder bump
    // we'd do now would be wiped by the next launch. Skip the Python
    // checks while an app update is pending.
    // The dev channel skips automatic update checks entirely. When
    // the active backend is a builder variant, the
    // `esphome-device-builder` package is checked on the same schedule.
    let update_state = Arc::clone(state);
    let update_app = app_handle.clone();
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
            update_state
                .update_checker
                .check_and_notify_device_builder(&update_app, backend, update_tray_available)
                .await;
        }
    });
}

/// Trip a clean exit on SIGINT/SIGTERM. No-op off Unix.
#[cfg_attr(not(unix), allow(unused_variables))]
pub(super) fn spawn_signal_handlers(app_handle: &AppHandle) {
    // The daemon-stop is handled by the `RunEvent::ExitRequested`
    // branch in `crate::shutdown`; we just trip the exit here.
    #[cfg(unix)]
    {
        let signal_app = app_handle.clone();
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
}

/// Open the dashboard once the backend answers, when startup calls for it.
pub(super) fn open_dashboard_on_start(
    state: &Arc<AppState>,
    tray_available: bool,
    no_open_dashboard: bool,
) {
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
}
