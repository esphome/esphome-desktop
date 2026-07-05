//! Local control channel: `esphome-desktop <subcommand>` driving the running
//! app over a unix socket (macOS/Linux) or named pipe (Windows).
//!
//! The tray menu is the app's only built-in control surface, and on Linux
//! systems without a StatusNotifier host it never appears. The CLI mirrors
//! the tray's actions so those systems can still open the dashboard, switch
//! channels, update, restart, and quit.

pub mod client;
pub mod ops;
pub mod protocol;
pub mod server;

/// Absolute path used to invoke the `esphome-desktop` CLI (its own `api`
/// subcommands). Handed to the device-builder child as `ESPHOME_DESKTOP_BIN` so
/// the dashboard can query and trigger updates through the stable `api`
/// interface without guessing where the binary lives.
///
/// On Linux under an AppImage the stable path is `$APPIMAGE`; `current_exe`
/// there points inside the FUSE mount, which is unmounted once the process that
/// owns it exits, so a path derived from it can't be re-run later. On
/// macOS/Windows `current_exe` is the bundle's own binary, which dispatches
/// subcommands directly (the same binary the macOS PATH wrapper execs).
pub(crate) fn cli_invocation_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        if !appimage.is_empty() {
            return Some(std::path::PathBuf::from(appimage));
        }
    }
    std::env::current_exe().ok()
}
