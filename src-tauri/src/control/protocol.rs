//! Wire protocol for the local control channel between the CLI client
//! (`esphome-desktop <subcommand>`) and the running app.
//!
//! Framing is newline-delimited JSON with one request per connection: the
//! client sends a single [`Request`] line; the server replies with zero or
//! more [`Reply::Progress`] lines followed by exactly one terminal reply
//! ([`Reply::Ok`], [`Reply::Err`], or [`Reply::Status`]) and closes the
//! connection.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::settings::{Backend, ReleaseChannel};

/// Upper bound on a single protocol line. Requests and replies are tiny; a
/// line this long means a confused peer, not a real client.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// Name of the control pipe on Windows, where unix sockets are unavailable.
#[cfg(windows)]
pub const PIPE_NAME: &str = r"\\.\pipe\io.esphome.builder.control";

/// Progress step emitted just before the app relaunches to finish a desktop
/// self-update. The relaunch tears the connection down, so the client treats
/// an EOF after this marker as success rather than a dropped connection.
pub const STEP_APP_RESTARTING: &str = "app_restarting";

/// A command sent by the CLI client to the running app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Open the dashboard in the default browser.
    Open,
    /// Report the active device-builder backend channel.
    GetBackend,
    /// Switch the device-builder backend channel.
    SetBackend { backend: Backend },
    /// Report the ESPHome release channel.
    GetChannel,
    /// Switch the ESPHome release channel.
    SetChannel { channel: ReleaseChannel },
    /// Report whether the app launches at login.
    GetStartup,
    /// Enable or disable launching at login.
    SetStartup { enable: bool },
    /// Update the desktop app, ESPHome, and the device builder.
    Update,
    /// Restart the dashboard backend.
    Restart,
    /// Quit the app.
    Quit,
    /// Report app and backend status.
    Status,
}

/// Error category for [`Reply::Err`], mapped to distinct CLI exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrCode {
    /// Another update/switch sequence holds the re-entrancy guard.
    Busy,
    /// The operation ran and failed.
    Failed,
}

/// A reply line from the server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Reply {
    /// Terminal: the operation succeeded.
    Ok { message: String },
    /// Terminal: the operation failed or was rejected.
    Err { message: String, code: ErrCode },
    /// Non-terminal progress note for long-running operations.
    Progress { step: String, detail: String },
    /// Terminal reply to [`Request::Status`].
    Status(Box<StatusReply>),
}

impl Reply {
    /// Terminal success reply.
    pub fn ok(message: impl Into<String>) -> Self {
        Reply::Ok {
            message: message.into(),
        }
    }

    /// Terminal failure reply with [`ErrCode::Failed`].
    pub fn failed(message: impl Into<String>) -> Self {
        Reply::Err {
            message: message.into(),
            code: ErrCode::Failed,
        }
    }
}

/// Full status snapshot returned for [`Request::Status`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusReply {
    pub app_version: String,
    pub backend_running: bool,
    /// Whether the dashboard actually answered an HTTP probe.
    pub backend_healthy: bool,
    pub port: u16,
    pub esphome_version: Option<String>,
    pub device_builder_version: Option<String>,
    pub release_channel: ReleaseChannel,
    pub backend: Backend,
    pub launch_at_startup: bool,
    pub config_dir: PathBuf,
    pub logs_dir: PathBuf,
}

/// Short lowercase channel name used on the CLI surface (matches the
/// `release-channel` argument values).
pub fn channel_name(channel: ReleaseChannel) -> &'static str {
    match channel {
        ReleaseChannel::Stable => "stable",
        ReleaseChannel::Beta => "beta",
        ReleaseChannel::Dev => "dev",
    }
}

/// Short lowercase backend channel name used on the CLI surface (matches the
/// `backend` argument values).
pub fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::BuilderStable => "stable",
        Backend::BuilderBeta => "beta",
    }
}

/// Path of the control socket, resolvable from both the app and the CLI
/// client (no `AppHandle`).
#[cfg(unix)]
pub fn socket_path() -> anyhow::Result<PathBuf> {
    let dir = control_dir()
        .ok_or_else(|| anyhow::anyhow!("could not resolve a directory for the control socket"))?;
    let path = dir.join("control.sock");
    validate_socket_path_len(&path)?;
    Ok(path)
}

/// Directory the control socket lives in; the server creates it with mode
/// `0700` so the socket is never reachable by other users, even in the
/// instant between `bind()` (which applies the umask) and the follow-up
/// chmod. Linux prefers `$XDG_RUNTIME_DIR` (per-user tmpfs, cleared on
/// logout/reboot, so a crash never leaves a stale file across boots); macOS,
/// and Linux without the variable, use a dedicated `control/` subdirectory of
/// the app data dir.
#[cfg(unix)]
fn control_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let dir = PathBuf::from(dir);
        // The spec mandates an absolute path; a relative one would resolve
        // against each process's own cwd, so the app and the CLI would end
        // up on different sockets.
        if dir.is_absolute() {
            return Some(dir.join(crate::platform::BUNDLE_IDENTIFIER));
        }
    }
    crate::platform::data_dir_no_handle().map(|dir| dir.join("control"))
}

/// `sockaddr_un.sun_path` holds ~104 bytes on macOS and 108 on Linux; binding
/// a longer path fails with an unhelpful OS error, so check up front with a
/// clear one.
#[cfg(unix)]
fn validate_socket_path_len(path: &std::path::Path) -> anyhow::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let len = path.as_os_str().as_bytes().len();
    if len > 100 {
        anyhow::bail!(
            "control socket path is too long for a unix socket ({} bytes): {}",
            len,
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let requests = vec![
            Request::Open,
            Request::GetBackend,
            Request::SetBackend {
                backend: Backend::BuilderStable,
            },
            Request::GetChannel,
            Request::SetChannel {
                channel: ReleaseChannel::Dev,
            },
            Request::GetStartup,
            Request::SetStartup { enable: false },
            Request::Update,
            Request::Restart,
            Request::Quit,
            Request::Status,
        ];
        for req in requests {
            let line = serde_json::to_string(&req).expect("serialize");
            let back: Request = serde_json::from_str(&line).expect("deserialize");
            assert_eq!(back, req, "line: {line}");
        }
    }

    #[test]
    fn reply_round_trips() {
        let replies = vec![
            Reply::Ok {
                message: "done".into(),
            },
            Reply::Err {
                message: "busy".into(),
                code: ErrCode::Busy,
            },
            Reply::Err {
                message: "boom".into(),
                code: ErrCode::Failed,
            },
            Reply::Progress {
                step: "esphome".into(),
                detail: "installing".into(),
            },
            Reply::Status(Box::new(StatusReply {
                app_version: "0.12.2".into(),
                backend_running: true,
                backend_healthy: false,
                port: 6052,
                esphome_version: Some("2026.6.2".into()),
                device_builder_version: None,
                release_channel: ReleaseChannel::Beta,
                backend: Backend::BuilderBeta,
                launch_at_startup: true,
                config_dir: PathBuf::from("/home/x/esphome"),
                logs_dir: PathBuf::from("/home/x/.local/share/io.esphome.builder/logs"),
            })),
        ];
        for reply in replies {
            let line = serde_json::to_string(&reply).expect("serialize");
            let back: Reply = serde_json::from_str(&line).expect("deserialize");
            assert_eq!(back, reply, "line: {line}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn socket_path_length_is_validated() {
        let short = std::path::Path::new("/tmp/io.esphome.builder/control.sock");
        assert!(validate_socket_path_len(short).is_ok());

        let long = std::path::Path::new(
            "/very/long/prefix/that/goes/on/and/on/and/on/well/past/the/sun/path/limit/of/a/unix/domain/socket/control.sock",
        );
        assert!(validate_socket_path_len(long).is_err());
    }
}
