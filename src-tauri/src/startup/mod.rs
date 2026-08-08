//! Everything Tauri's `setup` hook runs: environment preparation, app
//! state, the control server, the tray, and the background tasks.
//!
//! Ordering is load-bearing throughout and each step documents its own
//! constraint; the short version is that anything the daemon child
//! inherits (PATH, the user Python tree, persisted settings) has to be in
//! place before [`tasks::spawn_daemon_start`] is reached.

mod tasks;

use std::sync::Arc;
use tauri::{async_runtime, Manager};
use tauri_plugin_autostart::ManagerExt;
use tracing::{error, info, warn};

use crate::settings::Backend;
use crate::{control, platform, settings, tray, AppState};

/// Run the setup hook. See the module docs for the ordering contract.
pub(crate) fn setup(
    app: &mut tauri::App,
    cli_backend_override: Option<Backend>,
    no_open_dashboard: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Setting up ESPHome Device Builder");

    // Users migrating off the removed classic dashboard backend should
    // land on the fresh bundled device builder, not a stale pip-pinned
    // copy. Detect the classic selection from the persisted settings
    // before the bundled-Python refresh so the refresh can skip
    // preserving an old `esphome-device-builder` version.
    let refresh_reason = if settings::persisted_backend_was_classic(app.handle()) {
        platform::RefreshReason::ClassicMigration
    } else {
        platform::RefreshReason::Startup
    };

    // Ensure user Python exists (copied from the bundle on first run).
    // This must happen before AppState::new() so paths are correct
    if let Err(e) = platform::ensure_user_python(app.handle(), refresh_reason) {
        error!("Failed to set up user Python: {}", e);
        // Continue anyway - might work with bundled Python
    }

    // Make a git available to the ESPHome backend. On Windows this
    // always prepends the bundled MinGit to PATH; no-op elsewhere. Runs
    // before the daemon task spawns so the child (which inherits this
    // process's PATH) and the missing-git check both observe it.
    // Log-and-continue: a failure here only means git-dependent
    // features fall back to the existing notification.
    if let Err(e) = platform::ensure_git_on_path(app.handle()) {
        error!("Failed to set up bundled git: {}", e);
    }

    // Append Homebrew's bin dirs to PATH on macOS so ESP-IDF builds can
    // find a brew-installed `ccache` (the GUI/login-item session PATH
    // excludes Homebrew). Appended, so it never shadows system/bundled
    // tools. No-op elsewhere; must run before the daemon task spawns so
    // the child inherits the augmented PATH. Log-and-continue.
    if let Err(e) = platform::ensure_homebrew_on_path(app.handle()) {
        error!("Failed to add Homebrew to PATH: {}", e);
    }

    // Make the bundled ccache discoverable to the ESPHome backend on
    // Windows so ESP-IDF builds enable compiler caching automatically.
    // Prepends to PATH like the git setup above; no-op elsewhere. Runs
    // before the daemon task spawns so the child inherits it.
    // Log-and-continue: builds just run without caching on failure.
    if let Err(e) = platform::ensure_ccache_on_path(app.handle()) {
        error!("Failed to set up bundled ccache: {}", e);
    }

    // One-shot prompt to remove the pre-rename `/Applications/ESPHome Builder.app`.
    // No-op on non-macOS and after the user has answered once.
    platform::cleanup_legacy_macos_app(app.handle());

    // Perform platform-specific initialization
    platform::init(app.handle());

    // Initialize app state
    let state = Arc::new(AppState::new(app.handle())?);
    app.manage(state.clone());

    // Start the local control server so `esphome-desktop <subcommand>`
    // can drive this instance — the only control surface on systems
    // where the tray is unavailable.
    control::server::spawn(app.handle().clone());

    // If we just migrated a classic-backend user, persist the migrated
    // settings (loaded as the default device builder) so the legacy
    // value is cleared from disk and a later app update won't re-force.
    if refresh_reason == platform::RefreshReason::ClassicMigration {
        let settings = async_runtime::block_on(state.settings.read());
        if let Err(e) = settings.save(app.handle()) {
            warn!("Failed to persist backend migration: {}", e);
        }
    }

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
            // Changing the channel needs a (re)install of the package.
            true
        } else {
            false
        }
    } else {
        false
    };

    // Reconcile the OS login item to the persisted preference. This
    // applies the on-by-default on first run and re-asserts a user's
    // choice on every launch (so an "off" sticks and drift self-heals).
    {
        let want = async_runtime::block_on(state.settings.read()).launch_at_startup;
        let manager = app.autolaunch();
        match manager.is_enabled() {
            Ok(current) if current != want => {
                let result = if want {
                    manager.enable()
                } else {
                    manager.disable()
                };
                if let Err(e) = result {
                    warn!("Failed to set autostart to {}: {}", want, e);
                }
            }
            Err(e) => warn!("Failed to query autostart state: {}", e),
            _ => {}
        }
    }

    // Build and set up the tray menu (if tray support is available)
    let tray_available = tray::icon::create(app, &state);

    tasks::spawn_daemon_start(app.handle(), &state, cli_override_needs_install);
    tasks::spawn_update_checker(app.handle(), &state, tray_available);
    tasks::spawn_signal_handlers(app.handle());
    tasks::open_dashboard_on_start(&state, tray_available, no_open_dashboard);

    Ok(())
}
