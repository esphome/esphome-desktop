//! Command-line interface: the clap argument model plus the pre-parse launch
//! helpers `main` calls before Tauri starts.
//!
//! The types here are the contract the binary and the control client share —
//! [`Cli`] is parsed in `main`, and [`CliCommand`]/[`ApiMethod`]/[`OnOff`] are
//! matched by [`control::client`](crate::control::client). They are re-exported
//! at the crate root, so external paths (`esphome_desktop_lib::Cli`,
//! `crate::CliCommand`) keep working after this split.

use clap::Parser;

use crate::settings::{self, Backend};

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
/// app. See [`control`](crate::control).
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
/// [`control::protocol::API_SCHEMA_VERSION`](crate::control::protocol), so the
/// human CLI stays free to change. Every line is one JSON object the caller can
/// `json.loads`.
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
    crate::control::client::run(command)
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

#[cfg(test)]
mod tests {
    use super::is_bare_terminal_launch;

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
}
