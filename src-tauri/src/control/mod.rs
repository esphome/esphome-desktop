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
