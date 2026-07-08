//! ESPHome Device Builder Application
//!
//! A cross-platform desktop application that manages ESPHome as a background daemon
//! with system tray integration.

mod app_update;
mod control;
mod daemon;
mod dialog;
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
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
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

impl From<BuilderChannelArg> for Backend {
    fn from(arg: BuilderChannelArg) -> Self {
        match arg {
            BuilderChannelArg::Stable => Backend::BuilderStable,
            BuilderChannelArg::Beta => Backend::BuilderBeta,
        }
    }
}

/// CLI selector for the ESPHome release channel.
/// Maps onto [`settings::ReleaseChannel`].
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "lowercase")]
pub enum ReleaseChannelArg {
    Stable,
    Beta,
    Dev,
}

impl From<ReleaseChannelArg> for settings::ReleaseChannel {
    fn from(arg: ReleaseChannelArg) -> Self {
        match arg {
            ReleaseChannelArg::Stable => Self::Stable,
            ReleaseChannelArg::Beta => Self::Beta,
            ReleaseChannelArg::Dev => Self::Dev,
        }
    }
}

/// CLI selector for a boolean setting (`startup on` / `startup off`).
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "lowercase")]
pub enum OnOff {
    On,
    Off,
}

/// Subcommands that control an already-running app over the local control
/// channel instead of launching a new instance. They mirror the tray menu so
/// systems without a working tray (some Linux desktops) can still drive the
/// app. See [`control`].
#[derive(clap::Subcommand, Debug, Clone)]
pub enum CliCommand {
    /// Open the dashboard in the default browser (starts the app if needed)
    Open,
    /// Show or switch the device-builder backend channel
    Backend {
        /// New backend channel; omit to show the current one
        #[arg(value_enum)]
        channel: Option<BuilderChannelArg>,
    },
    /// Show or switch the ESPHome release channel
    ReleaseChannel {
        /// New release channel; omit to show the current one
        #[arg(value_enum)]
        channel: Option<ReleaseChannelArg>,
    },
    /// Show or set whether the app launches at login
    Startup {
        /// New state; omit to show the current one
        #[arg(value_enum)]
        state: Option<OnOff>,
    },
    /// Update the desktop app, ESPHome, and the device builder
    Update,
    /// Show recent dashboard log output
    Logs {
        /// Keep streaming new log lines
        #[arg(short, long)]
        follow: bool,
        /// Open the logs folder in the file manager instead
        #[arg(long)]
        open: bool,
    },
    /// Restart the dashboard backend
    Restart,
    /// Quit the running app
    Quit,
    /// Show app and backend status
    Status {
        /// Print the status as JSON
        #[arg(long)]
        json: bool,
    },
    /// Stable, versioned JSON API for the device-builder integration (hidden
    /// from help; not for interactive use). Emits newline-delimited JSON only.
    #[command(subcommand, hide = true)]
    Api(ApiMethod),
}

/// Methods of the machine-readable `esphome-desktop api <method>` interface.
/// This is the contract the device-builder dashboard codes against; unlike the
/// human subcommands above it emits only NDJSON and is versioned via
/// [`control::protocol::API_SCHEMA_VERSION`], so the human CLI stays free to
/// change. Every line is one JSON object the caller can `json.loads`.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum ApiMethod {
    /// Print the API schema version and exit (no running app required)
    Version,
    /// Print app and backend status as one JSON object
    Status,
    /// Report whether any component has an update available, without installing
    CheckUpdate,
    /// Trigger the full update; streams JSON progress then a terminal reply.
    /// Non-interactive: the backend is restarted without any confirmation, so
    /// an unattended remote builder recovers on its own.
    Update,
}

/// ESPHome Device Builder - System tray application for ESPHome
#[derive(Parser, Debug, Clone)]
#[command(name = "esphome-desktop")]
#[command(about = "ESPHome Device Builder", long_about = None)]
#[command(
    after_help = "Run 'esphome-desktop open' to start the app and open the dashboard; \
                  launching with no subcommand starts the app when run outside a terminal."
)]
pub struct Cli {
    /// Control an already-running app instead of launching one.
    #[command(subcommand)]
    pub command: Option<CliCommand>,

    /// Don't open the dashboard in browser on startup
    #[arg(long = "no-open-dashboard")]
    pub no_open_dashboard: bool,

    /// Apply the `--builder-channel` selection (stable or beta) to the device
    /// builder. Persists to settings — useful as a fallback when the tray menu
    /// is unavailable.
    #[arg(long = "use-builder")]
    pub use_builder: bool,

    /// Channel for the ESPHome Device Builder backend.
    /// Only takes effect together with `--use-builder`.
    #[arg(long = "builder-channel", value_enum, default_value_t = BuilderChannelArg::Beta)]
    pub builder_channel: BuilderChannelArg,
}

/// Run a control subcommand as a short-lived CLI client and return its exit
/// code. No Tauri, no logging init — this path must stay quiet and quick.
pub fn run_cli(command: CliCommand) -> std::process::ExitCode {
    control::client::run(command)
}

/// Whether this is a bare `esphome-desktop` run from a terminal — no
/// subcommand and no flags at all, just the program name — which should print
/// the command list instead of launching another app instance. Any explicit
/// argument is a deliberate invocation and launches as before: a launch flag
/// like `--no-open-dashboard`, or even the no-op `--builder-channel`, so the
/// rule needs no per-flag list and stays correct as flags are added.
/// Non-terminal launches (Finder, the applications menu, a `.desktop` file,
/// autostart, `open`'s detached spawn) also take the normal app-start path.
///
/// `from_terminal` is the platform's "started from a console" signal (a real
/// TTY on Unix, a successful parent-console attach on Windows — see
/// [`attach_parent_console`]). `arg_count` is `std::env::args_os().count()`,
/// so the bare case is a count of 1 (just the program name).
pub fn is_bare_terminal_launch(from_terminal: bool, arg_count: usize) -> bool {
    from_terminal && arg_count <= 1
}

/// Attach to the parent process's console so terminal output is visible, and
/// report whether one was attached. Release builds use
/// `windows_subsystem = "windows"`, which starts the process with no console,
/// so `--help` and usage errors would otherwise print nowhere; this must run
/// before clap parses. `AttachConsole(ATTACH_PARENT_PROCESS)` succeeds only
/// when the launcher had a console (cmd/PowerShell) and fails on a GUI /
/// Start-menu / autostart launch, so its result is also the reliable "started
/// from a terminal" signal — more robust than reading the std handles with
/// `is_terminal()` afterward, which is not guaranteed to observe the
/// just-attached console.
#[cfg(windows)]
pub fn attach_parent_console() -> bool {
    use ::windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS).is_ok() }
}

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
pub(crate) fn updates_menu_hint(tray_available: bool) -> &'static str {
    if tray_available {
        "Open the tray menu and choose \"Check for Updates...\" to update."
    } else {
        "No system tray was detected. Run `esphome-desktop update` from a \
         terminal to update."
    }
}

/// Open the ESPHome dashboard in the default browser. Detached: `open::that`
/// waits for the opener process to exit, which can block the calling thread
/// (including a tokio worker when invoked from the control server).
pub(crate) fn open_dashboard(port: u16) {
    let url = format!("http://localhost:{}", port);
    if let Err(e) = open::that_detached(&url) {
        error!("Failed to open browser: {}", e);
    }
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

    let url = daemon::loopback_url(port);
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

/// Number of rotated app-log files to retain (one per day of activity).
const APP_LOG_HISTORY: usize = 7;

/// Build the rolling app-level log appender (`<data>/logs/app.<date>.log`).
///
/// Resolved without an `AppHandle` (logging is initialised before Tauri builds
/// one) using the same bundle identifier Tauri's `app_data_dir()` uses, so this
/// sits next to the dashboard logs and stays inspectable across a self-update
/// restart — issue #203. Daily rotation with [`APP_LOG_HISTORY`] retained keeps
/// it bounded even when the filter is raised to `debug` to chase a failure.
/// Best-effort: returns None if the dir or appender can't be built, leaving
/// stderr logging.
fn app_log_appender() -> Option<tracing_appender::rolling::RollingFileAppender> {
    let dir = platform::data_dir_no_handle()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("app")
        .filename_suffix("log")
        .max_log_files(APP_LOG_HISTORY)
        .build(dir)
        .ok()
}

/// Initialize logging
fn init_logging() {
    // Optional rolling file layer beside stderr: a no-op when the appender can't
    // be built, so a path failure never blocks startup. The appender is its own
    // `MakeWriter`, so there's no per-event handle clone and no panic path in
    // the logging hot loop.
    let file_layer = app_log_appender().map(|appender| {
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(appender)
    });

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "esphome_desktop=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .with(file_layer)
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
        .setup(move |app| {
            info!("Setting up ESPHome Device Builder");

            // Users migrating off the removed classic dashboard backend should
            // land on the fresh bundled device builder, not a stale pip-pinned
            // copy. Detect the classic selection from the persisted settings
            // before the bundled-Python refresh so the refresh can skip
            // preserving an old `esphome-device-builder` version.
            let force_device_builder = settings::persisted_backend_was_classic(app.handle());

            // Ensure user Python exists (copy from bundled on first run for non-Windows)
            // This must happen before AppState::new() so paths are correct
            if let Err(e) = platform::ensure_user_python(app.handle(), force_device_builder) {
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
            if force_device_builder {
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
            let tray_available = if platform::is_tray_supported() {
                // Create the tray icon programmatically.
                // We wrap this in catch_unwind as a safety net: on Linux the
                // underlying libappindicator-sys crate will panic!() if the
                // shared library fails to load (e.g. GLIBC version mismatch).
                let tray_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // The macOS menu bar expects a monochrome "template" image
                    // whose alpha channel the system recolors to match the
                    // light/dark theme; other platforms use the full-color
                    // bundled icon.
                    #[cfg(target_os = "macos")]
                    let (icon, icon_as_template) = (
                        tauri::image::Image::from_bytes(include_bytes!("../icons/tray-mac.png"))?,
                        true,
                    );
                    #[cfg(not(target_os = "macos"))]
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
                // The control server is already accepting requests, so an
                // early CLI `update`/`release-channel` could otherwise
                // interleave its stop→install→start with this initial
                // install/start (e.g. its stop() no-ops before our start()
                // spawns the old backend mid-install). Hold the same guard
                // the update/switch sequences use; at startup it is almost
                // always free, so this settles immediately.
                let startup_guard =
                    control::ops::UpdateGuard::acquire_wait(daemon_state.update_in_flight.clone())
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
                    update_state
                        .update_checker
                        .check_and_notify_device_builder(
                            &update_app,
                            backend,
                            update_tray_available,
                        )
                        .await;
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
        });
}

#[cfg(test)]
mod tests {
    use super::is_bare_terminal_launch;
    use super::updates_menu_hint;

    #[test]
    fn bare_run_in_a_terminal_shows_help() {
        // Just the program name (arg_count 1), attached to a terminal.
        assert!(is_bare_terminal_launch(true, 1));
    }

    #[test]
    fn bare_run_without_a_terminal_launches() {
        // Finder / autostart / detached spawn: no terminal, so start the app.
        assert!(!is_bare_terminal_launch(false, 1));
    }

    #[test]
    fn any_argument_launches_even_in_a_terminal() {
        // A launch flag, a subcommand, or even the no-op `--builder-channel`
        // is a deliberate invocation: arg_count > 1, so never bare.
        assert!(!is_bare_terminal_launch(true, 2)); // e.g. --no-open-dashboard
        assert!(!is_bare_terminal_launch(true, 3)); // e.g. --builder-channel stable
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
        // Must offer a concrete alternative: the CLI update command.
        assert!(hint.contains("esphome-desktop update"));
    }
}
