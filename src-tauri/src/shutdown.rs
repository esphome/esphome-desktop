//! The `RunEvent` handler: tear the dashboard child down before the
//! process goes away.

use std::sync::Arc;
use tauri::{async_runtime, AppHandle, Manager, RunEvent};
use tracing::{info, warn};

use crate::{control, AppState};

/// Handle one runtime event. Wired as the callback to `App::run`.
pub(crate) fn on_run_event(app_handle: &AppHandle, event: RunEvent) {
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

    // The process is going away; remove the control socket file so the
    // next launch binds cleanly (best-effort — a SIGKILL skips this,
    // which the server's stale-socket check covers).
    if matches!(event, RunEvent::Exit) {
        control::server::cleanup();
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
}
