//! Tray icon creation and its left-click handler.
//!
//! Split from the crate root so the tray module owns its whole surface —
//! icon, menu ([`super::build_tray_menu`]), and events — and startup only
//! asks whether a tray exists.

use std::sync::Arc;
use tauri::{
    async_runtime,
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle,
};
use tracing::{info, warn};

use super::build_tray_menu;
use crate::{open_dashboard, platform, AppState};

/// Build the tray icon and attach the menu and click handler.
///
/// Returns whether a tray is actually available: `false` both when the
/// platform has no StatusNotifier host and when creation fails or panics,
/// which is what makes the app fall back to opening the dashboard and to
/// the CLI update hint (issue #87).
pub(crate) fn create(app: &tauri::App, state: &Arc<AppState>) -> bool {
    if platform::is_tray_supported() {
        // Create the tray icon programmatically.
        // We wrap this in catch_unwind as a safety net: on Linux the
        // underlying libappindicator-sys crate will panic!() if the
        // shared library fails to load (e.g. GLIBC version mismatch).
        let tray_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // The macOS menu bar expects a monochrome "template" image
            // whose alpha channel the system recolors to match the
            // light/dark theme. Linux tray pixmaps are rendered
            // literally with no theme recoloring, so it gets a fixed
            // white glyph suited to the (near-universal) dark panels.
            // Windows keeps the full-color bundled icon.
            #[cfg(target_os = "macos")]
            let (icon, icon_as_template) = (
                tauri::image::Image::from_bytes(include_bytes!("../../icons/tray-mac.png"))?,
                true,
            );
            #[cfg(target_os = "linux")]
            let (icon, icon_as_template) = (
                tauri::image::Image::from_bytes(include_bytes!("../../icons/tray-linux.png"))?,
                false,
            );
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let (icon, icon_as_template) = (
                app.default_window_icon()
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("No default icon available for tray"))?,
                false,
            );

            let tray = TrayIconBuilder::with_id("main")
                .icon(icon)
                .icon_as_template(icon_as_template)
                .tooltip("ESPHome Device Builder")
                .build(app)?;

            let menu = build_tray_menu(app.handle(), state)?;
            tray.set_menu(Some(menu))?;

            // Set up click handler
            let state_clone = Arc::clone(state);
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
    }
}

/// Handle tray icon left-click (open dashboard)
fn handle_tray_click(_app: &AppHandle, state: &AppState) {
    let settings = async_runtime::block_on(state.settings.read());
    open_dashboard(settings.port);
}
