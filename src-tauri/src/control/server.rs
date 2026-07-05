//! In-app side of the control channel.
//!
//! Listens on a unix domain socket (macOS/Linux) or a named pipe (Windows)
//! and dispatches CLI requests onto the same operations the tray menu uses.
//! This is what makes the app controllable on Linux systems without a
//! working system tray.

use std::sync::Arc;
use tauri::{AppHandle, Manager};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{info, warn};

use super::ops::{self, SwitchOutcome, UpdateGuard};
use super::protocol::{
    self, backend_name, channel_name, ErrCode, Reply, Request, StatusReply, UpdateCheckReply,
};
use crate::AppState;

/// Backoff before retrying a failed accept or pipe re-create, so a
/// persistent error can't become a tight spin.
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Action the connection handler performs after the terminal reply has been
/// flushed and the connection closed — both tear the process (and with it the
/// socket) down, so they must not run before the client has its answer.
enum PostAction {
    /// `quit`: exit the app through the normal shutdown path.
    Exit,
    /// A desktop self-update was installed; relaunch to finish it.
    Relaunch,
}

/// Spawn the control server onto the async runtime. Failures are logged and
/// leave the app running without a control channel; they never block startup.
pub fn spawn(app_handle: AppHandle) {
    tauri::async_runtime::spawn(async move {
        run(app_handle).await;
    });
}

/// Path of the socket THIS process bound, if any. Cleanup must only remove a
/// socket we own: a second app instance whose control server was disabled
/// ("already in use") still exits through `RunEvent::Exit`, and removing the
/// path by mere derivation would delete the primary instance's live socket
/// out from under it — the app then keeps running but the CLI reports it as
/// not running until it is restarted.
#[cfg(unix)]
static BOUND_SOCKET: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Best-effort removal of the socket file at shutdown (`RunEvent::Exit`).
#[cfg(unix)]
pub fn cleanup() {
    if let Some(path) = BOUND_SOCKET.get() {
        let _ = std::fs::remove_file(path);
    }
}

/// Named pipes disappear with the process; nothing to clean up.
#[cfg(windows)]
pub fn cleanup() {}

#[cfg(unix)]
async fn run(app: AppHandle) {
    if let Err(e) = serve(app).await {
        warn!("Control server disabled: {:#}", e);
    }
}

#[cfg(unix)]
async fn serve(app: AppHandle) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    let path = protocol::socket_path()?;
    if let Some(parent) = path.parent() {
        // Same-user only: the request surface includes quit/update/switch.
        // The directory is created (and kept) 0700 so the socket inside is
        // unreachable by other users even during the brief window between
        // bind() — which applies the umask — and the chmod below.
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        builder.mode(0o700);
        builder
            .create(parent)
            .with_context(|| format!("could not create {parent:?}"))?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("could not restrict permissions on {parent:?}"))?;
    }
    // Stale-socket handling: the single-instance plugin guarantees only one
    // app instance, so a live socket here means some other process owns the
    // path — leave it alone. A dead file (a crash or SIGKILL skipped the exit
    // cleanup) is removed so bind() succeeds.
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(&path) {
            Ok(_) => anyhow::bail!("control socket {path:?} is already in use"),
            Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let listener = tokio::net::UnixListener::bind(&path)
        .with_context(|| format!("could not bind {path:?}"))?;
    // We own the socket from here; only now may exit cleanup remove it.
    let _ = BOUND_SOCKET.set(path.clone());
    // Belt and suspenders on top of the 0700 parent directory above.
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        warn!("Could not restrict control socket permissions: {}", e);
    }
    info!("Control server listening on {:?}", path);

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    handle_connection(app, stream).await;
                });
            }
            Err(e) => {
                warn!("Control socket accept failed: {}", e);
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

#[cfg(windows)]
async fn run(app: AppHandle) {
    use tokio::net::windows::named_pipe::ServerOptions;

    let mut first = true;
    loop {
        // A fresh server instance per client; `first_pipe_instance` makes the
        // initial create fail fast if another process already owns the name.
        //
        // Access control (the unix side locks its socket to the user with
        // file modes): no explicit DACL is set, so the pipe gets the creating
        // token's default DACL, which grants access to the creator, SYSTEM,
        // and Administrators only, and tokio's `ServerOptions` rejects remote
        // clients by default — effectively same-user, matching the unix side.
        let server = match ServerOptions::new()
            .first_pipe_instance(first)
            .create(protocol::PIPE_NAME)
        {
            Ok(s) => s,
            Err(e) if first => {
                // The name is taken (another process owns it) or pipes are
                // unavailable; nothing to serve.
                warn!(
                    "Control server disabled: could not create pipe {}: {}",
                    protocol::PIPE_NAME,
                    e
                );
                return;
            }
            Err(e) => {
                // We own the name; a later create failure is transient
                // (e.g. resource pressure) — retry rather than permanently
                // disabling the control channel.
                warn!("Control pipe re-create failed: {}", e);
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
        };
        if first {
            info!("Control server listening on {}", protocol::PIPE_NAME);
            first = false;
        }
        if let Err(e) = server.connect().await {
            warn!("Control pipe connect failed: {}", e);
            // Don't spin on a persistent failure; mirrors the unix accept path.
            tokio::time::sleep(RETRY_DELAY).await;
            continue;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            handle_connection(app, server).await;
        });
    }
}

/// Serve one connection: read a single request line, dispatch it, stream the
/// replies, close, then perform any post-close action (quit/relaunch).
async fn handle_connection<S>(app: AppHandle, stream: S)
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);

    // Cap the request line so a confused peer can't buffer unbounded data,
    // and time the read out so a silent connection can't park this task (and
    // hold the fd) forever.
    let mut reader = BufReader::new(read_half).take((protocol::MAX_LINE_BYTES + 1) as u64);
    let mut line = String::new();
    let read = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        reader.read_line(&mut line),
    )
    .await;
    let request: Request = match read {
        Err(_) => {
            warn!("Control request read timed out");
            return;
        }
        Ok(Ok(0)) => return, // client vanished before sending anything
        Ok(Ok(_)) if line.len() > protocol::MAX_LINE_BYTES => {
            let _ = write_reply(&mut write_half, &Reply::failed("request too large")).await;
            return;
        }
        Ok(Ok(_)) => match serde_json::from_str(line.trim()) {
            Ok(request) => request,
            Err(e) => {
                let _ = write_reply(
                    &mut write_half,
                    &Reply::failed(format!("invalid request: {e}")),
                )
                .await;
                return;
            }
        },
        Ok(Err(e)) => {
            warn!("Control request read failed: {}", e);
            return;
        }
    };

    // Replies flow through a channel to a dedicated writer task so
    // long-running handlers can stream Progress lines while they work.
    let (tx, mut rx) = mpsc::unbounded_channel::<Reply>();
    let writer = tauri::async_runtime::spawn(async move {
        while let Some(reply) = rx.recv().await {
            if write_reply(&mut write_half, &reply).await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    let post = dispatch(&app, request, &tx).await;

    // Close the channel and wait for the writer to flush everything the
    // handler sent; only then is it safe to tear the process down.
    drop(tx);
    let _ = writer.await;

    match post {
        Some(PostAction::Exit) => {
            info!("Quit requested via control channel");
            // Delegate cleanup to the RunEvent::ExitRequested handler in
            // lib.rs so the shutdown sequence lives in exactly one place.
            app.exit(0);
        }
        Some(PostAction::Relaunch) => {
            info!("Relaunching to apply desktop update (control channel)");
            crate::platform::relaunch_for_update(&app);
        }
        None => {}
    }
}

/// Serialize one reply as an NDJSON line and flush it.
async fn write_reply<W: AsyncWrite + Unpin>(writer: &mut W, reply: &Reply) -> std::io::Result<()> {
    let mut buf = serde_json::to_vec(reply).map_err(std::io::Error::other)?;
    buf.push(b'\n');
    writer.write_all(&buf).await?;
    writer.flush().await
}

/// Execute one request, sending progress and the terminal reply through `tx`.
async fn dispatch(
    app: &AppHandle,
    request: Request,
    tx: &mpsc::UnboundedSender<Reply>,
) -> Option<PostAction> {
    let Some(state) = app.try_state::<Arc<AppState>>() else {
        let _ = tx.send(Reply::failed(
            "the app is still starting up; try again shortly",
        ));
        return None;
    };
    let state: Arc<AppState> = state.inner().clone();

    /// Acquire the `UpdateGuard` or reply busy and bail out of `dispatch`,
    /// mirroring the tray arms' `guard_or_return!`.
    macro_rules! guard_or_busy {
        () => {
            match UpdateGuard::try_acquire(state.update_in_flight.clone()) {
                Some(guard) => guard,
                None => {
                    let _ = tx.send(Reply::Err {
                        message: "another update or switch is already in progress".to_string(),
                        code: ErrCode::Busy,
                    });
                    return None;
                }
            }
        };
    }

    let progress = |step: &str, detail: &str| {
        let _ = tx.send(Reply::Progress {
            step: step.to_string(),
            detail: detail.to_string(),
        });
    };

    match request {
        Request::Open => {
            let port = state.settings.read().await.port;
            crate::open_dashboard(port);
            let _ = tx.send(Reply::ok(format!("opening http://localhost:{port}")));
        }
        Request::GetChannel => {
            let channel = state.settings.read().await.release_channel;
            let _ = tx.send(Reply::ok(channel_name(channel)));
        }
        Request::SetChannel { channel } => {
            let guard = guard_or_busy!();
            let outcome =
                ops::switch_release_channel(app, &state, channel, &guard, &progress).await;
            let _ = tx.send(switch_reply(
                outcome,
                format!("release channel is already {}", channel_name(channel)),
                format!("switched to the {} release channel", channel_name(channel)),
            ));
        }
        Request::GetBackend => {
            let backend = state.settings.read().await.backend;
            let _ = tx.send(Reply::ok(backend_name(backend)));
        }
        Request::SetBackend { backend } => {
            let guard = guard_or_busy!();
            let outcome = ops::switch_backend(app, &state, backend, &guard, &progress).await;
            let _ = tx.send(switch_reply(
                outcome,
                format!("backend is already {}", backend_name(backend)),
                format!("switched to the {} device builder", backend_name(backend)),
            ));
        }
        Request::GetStartup => {
            let fallback = state.settings.read().await.launch_at_startup;
            let enabled = ops::startup_enabled(app, fallback).await;
            let _ = tx.send(Reply::ok(if enabled { "on" } else { "off" }));
        }
        Request::SetStartup { enable } => {
            let actual = ops::set_launch_at_startup(app, &state, enable).await;
            let verb = if enable { "enable" } else { "disable" };
            let reply = if actual == enable {
                Reply::ok(format!("launch at login {verb}d"))
            } else {
                Reply::failed(format!(
                    "tried to {verb} launch at login, but the OS reports it is {}",
                    if actual { "enabled" } else { "disabled" }
                ))
            };
            let _ = tx.send(reply);
        }
        Request::Update => {
            let guard = guard_or_busy!();
            let report = ops::run_full_update(app, &state, &guard, &progress).await;
            let summary = report.lines.join("; ");
            if report.app_update_installed {
                progress(
                    protocol::STEP_APP_RESTARTING,
                    "restarting to finish the desktop update",
                );
                let _ = tx.send(Reply::ok(summary));
                // Keep the in-flight flag held for the rest of this process's
                // life: the relaunch is imminent, and releasing it would let a
                // concurrent update/switch start a pip install that the exit
                // then orphans mid-write.
                std::mem::forget(guard);
                return Some(PostAction::Relaunch);
            }
            let _ = tx.send(if report.any_failed {
                Reply::failed(summary)
            } else {
                Reply::ok(summary)
            });
        }
        Request::Restart => {
            let guard = guard_or_busy!();
            match ops::restart_daemon(app, &state, true, &guard, &progress).await {
                Ok(true) => {
                    let _ = tx.send(Reply::ok("dashboard restarted and ready"));
                }
                Ok(false) => {
                    let _ = tx.send(Reply::failed(format!(
                        "dashboard restarted but {}",
                        ops::not_ready_note()
                    )));
                }
                Err(e) => {
                    let _ = tx.send(Reply::failed(format!(
                        "failed to restart the dashboard: {e}"
                    )));
                }
            }
        }
        Request::Quit => {
            // Refuse to quit while an update/switch is in flight: tearing the
            // process down now would orphan a pip install mid-write and corrupt
            // the site-packages tree — the same hazard the Update arm's
            // `mem::forget` note guards against. Keep the flag held for the rest
            // of this process's life (like the Update arm): dropping it here
            // would reopen the window during the teardown (writer flush + async
            // `daemon.stop()`) for a concurrent update to start a pip install
            // that the imminent exit then orphans.
            let guard = guard_or_busy!();
            let _ = tx.send(Reply::ok("quitting"));
            std::mem::forget(guard);
            return Some(PostAction::Exit);
        }
        Request::CheckUpdate => {
            let check = build_update_check(app, &state).await;
            let _ = tx.send(Reply::UpdateCheck(Box::new(check)));
        }
        Request::Status => {
            let status = build_status(app, &state).await;
            let _ = tx.send(Reply::Status(Box::new(status)));
        }
    }
    None
}

/// Map a [`SwitchOutcome`] onto the terminal reply for a set-channel or
/// set-backend request.
fn switch_reply(outcome: SwitchOutcome, unchanged_msg: String, success_msg: String) -> Reply {
    match outcome {
        SwitchOutcome::Unchanged => Reply::ok(unchanged_msg),
        SwitchOutcome::Success { ready: true } => Reply::ok(success_msg),
        SwitchOutcome::Success { ready: false } => Reply::failed(format!(
            "{success_msg}, but the dashboard {}",
            ops::not_ready_note()
        )),
        SwitchOutcome::StopFailed(e) => Reply::failed(format!("failed to stop the dashboard: {e}")),
        SwitchOutcome::InstallFailed { error, restarted } => Reply::failed(if restarted {
            format!("install failed: {error}; the previous version was restarted")
        } else {
            format!("install failed: {error}; the dashboard could not be restarted")
        }),
        SwitchOutcome::StartFailed(e) => {
            Reply::failed(format!("installed, but the dashboard failed to start: {e}"))
        }
    }
}

/// Assemble the full status snapshot. Version detection spawns Python
/// subprocesses, so it runs on blocking threads.
async fn build_status(app: &AppHandle, state: &Arc<AppState>) -> StatusReply {
    let (port, release_channel, backend, launch_fallback) = {
        let settings = state.settings.read().await;
        (
            settings.port,
            settings.release_channel,
            settings.backend,
            settings.launch_at_startup,
        )
    };

    // The probe and the two Python version detections are independent; run
    // them concurrently so `status` pays the slowest, not the sum.
    let (backend_healthy, esphome_version, device_builder_version, launch_at_startup) = tokio::join!(
        async { crate::daemon::health_check(port).await.unwrap_or(false) },
        ops::detect(app, crate::update::installed_esphome_version),
        ops::detect(app, crate::update::get_installed_device_builder_version),
        ops::startup_enabled(app, launch_fallback),
    );
    let esphome_version = esphome_version.ok().flatten();
    let device_builder_version = device_builder_version.ok().flatten();

    StatusReply {
        app_version: app.package_info().version.to_string(),
        backend_running: state.daemon.is_running(),
        backend_healthy,
        port,
        esphome_version,
        device_builder_version,
        release_channel,
        backend,
        launch_at_startup,
        config_dir: state.daemon.config_dir().clone(),
        logs_dir: state.daemon.logs_dir().clone(),
    }
}

/// Check every component for an available update without installing anything.
/// Read-only, so it takes no [`UpdateGuard`] and is safe to run even while an
/// update is in flight. The three checks hit the network (GitHub, PyPI) and
/// spawn Python for the installed versions, so run them concurrently.
async fn build_update_check(app: &AppHandle, state: &Arc<AppState>) -> UpdateCheckReply {
    let (channel, backend) = {
        let settings = state.settings.read().await;
        (settings.release_channel, settings.backend)
    };
    let (app_update, esphome, device_builder) = tokio::join!(
        ops::desktop_update_available(app),
        ops::esphome_update_available(app, state, channel),
        ops::device_builder_update_available(app, state, backend),
    );
    UpdateCheckReply {
        any_available: app_update.available || esphome.available || device_builder.available,
        app: app_update,
        esphome,
        device_builder,
    }
}
