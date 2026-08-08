//! Launch-at-login: persist the preference and reconcile the OS login item.
//!
//! Split from the switch/update sequences because it shares none of their
//! machinery — no daemon stop/start, no [`super::UpdateGuard`] — only the
//! settings write helper and the tray label refresh.

use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;
use tracing::error;

use super::set_and_save;
use crate::{tray, AppState};

/// Serializes launch-at-login toggles: concurrent toggles (two fast tray
/// clicks, or tray + CLI) could otherwise run their OS enable/disable calls
/// in the opposite order of their settings writes, leaving the login item
/// contradicting the setting. A dedicated lock rather than the settings lock,
/// so a slow OS call doesn't block unrelated settings readers.
static STARTUP_TOGGLE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Persist the autostart preference, reconcile the OS login item, and refresh
/// the tray radio labels. Returns the actual post-call OS state, which is what
/// callers should report — enable/disable can fail (permissions, policy,
/// platform limits), and reporting the requested state would mislead.
pub(crate) async fn set_launch_at_startup(
    app_handle: &AppHandle,
    state: &Arc<AppState>,
    enable: bool,
) -> bool {
    let _toggle = STARTUP_TOGGLE.lock().await;
    set_and_save(app_handle, state, |settings| {
        if settings.launch_at_startup != enable {
            settings.launch_at_startup = enable;
            true
        } else {
            false
        }
    })
    .await;

    // Always (re)apply the OS call, even when the persisted value already
    // matches, so an already-selected choice retries a registration that
    // failed earlier (e.g. the startup reconcile) instead of no-opping.
    // The plugin's calls are blocking OS work (macOS can shell out to
    // System Events), so keep them off the async runtime.
    let app = app_handle.clone();
    let actual = tokio::task::spawn_blocking(move || {
        let manager = app.autolaunch();
        let result = if enable {
            manager.enable()
        } else {
            manager.disable()
        };
        if let Err(e) = result {
            error!(
                "Failed to {} autostart: {}",
                if enable { "enable" } else { "disable" },
                e
            );
        }
        // Fall back to the requested value only if the state query itself
        // fails. The persisted setting keeps the user's intent, so the
        // launch-time reconcile retries.
        manager.is_enabled().unwrap_or(enable)
    })
    .await
    .unwrap_or(enable);
    tray::update_startup_checks(actual);
    actual
}

/// Actual OS login-item state, falling back to the persisted intent when the
/// query fails. Runs the blocking plugin call off the async runtime.
pub(crate) async fn startup_enabled(app_handle: &AppHandle, fallback: bool) -> bool {
    let app = app_handle.clone();
    tokio::task::spawn_blocking(move || app.autolaunch().is_enabled().unwrap_or(fallback))
        .await
        .unwrap_or(fallback)
}
