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

/// The running AppImage's own path from `$APPIMAGE`, if set and non-empty.
/// `current_exe` is unusable under an AppImage: it points inside the FUSE
/// mount, which is unmounted once the process that owns it exits, so a path
/// derived from it can't be re-run later. Linux-only; `None` elsewhere (there
/// `current_exe` is the real binary).
pub(crate) fn appimage_path() -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(appimage) = std::env::var_os("APPIMAGE") {
        let path = std::path::PathBuf::from(appimage);
        if !path.as_os_str().is_empty() {
            return Some(path);
        }
    }
    None
}

/// Absolute path used to invoke the `esphome-desktop` CLI (its own `api`
/// subcommands). Handed to the device-builder child as `ESPHOME_DESKTOP_BIN` so
/// the dashboard can query and trigger updates through the stable `api`
/// interface without guessing where the binary lives. On macOS/Windows
/// `current_exe` is the bundle's own binary, which dispatches subcommands
/// directly (the same binary the macOS PATH wrapper execs).
pub(crate) fn cli_invocation_path() -> Option<std::path::PathBuf> {
    if let Some(path) = appimage_path() {
        // `$APPIMAGE` is normally absolute, but canonicalize defensively so a
        // relative value (rare wrapper launches) still resolves after the
        // backend changes its working directory; fall back to the raw path if
        // canonicalization fails.
        return Some(std::fs::canonicalize(&path).unwrap_or(path));
    }
    std::env::current_exe().ok()
}
