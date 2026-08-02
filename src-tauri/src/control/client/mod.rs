//! CLI side of the control channel (`esphome-desktop <subcommand>`).
//!
//! Runs without Tauri and without a tokio runtime: plain std sockets are all
//! a one-shot request/reply exchange needs. `logs` never touches the channel
//! at all — the log paths are deterministic from the bundle identifier.

#[cfg(windows)]
use std::io::Read;
use std::io::{BufRead, BufReader, Write};
use std::process::ExitCode;
use std::time::Duration;

use super::protocol::{
    backend_name, channel_name, ErrCode, Reply, Request, StatusReply, STEP_APP_RESTARTING,
};
use crate::{CliCommand, OnOff};

mod api;
mod logs;

/// The operation succeeded.
const EXIT_SUCCESS: u8 = 0;
/// The operation ran and failed.
const EXIT_FAILED: u8 = 1;
/// The app is not running (or the control channel is unreachable).
const EXIT_NOT_RUNNING: u8 = 3;
/// Another update/switch sequence is already in flight.
const EXIT_BUSY: u8 = 4;

/// Read timeout for quick requests.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// `restart` drains the child for up to 30s and waits up to 60s for readiness.
const RESTART_TIMEOUT: Duration = Duration::from_secs(180);
/// Switches and updates download and pip-install packages.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(600);

/// Entry point for all subcommands; returns the process exit code.
/// (On Windows, `main` has already attached the parent console.)
pub(crate) fn run(command: CliCommand) -> ExitCode {
    match command {
        CliCommand::Open => open_cmd(),
        CliCommand::Backend { channel } => match channel {
            None => simple(Request::GetBackend, DEFAULT_TIMEOUT),
            Some(channel) => simple(
                Request::SetBackend {
                    backend: channel.into(),
                },
                UPDATE_TIMEOUT,
            ),
        },
        CliCommand::ReleaseChannel { channel } => match channel {
            None => simple(Request::GetChannel, DEFAULT_TIMEOUT),
            Some(channel) => simple(
                Request::SetChannel {
                    channel: channel.into(),
                },
                UPDATE_TIMEOUT,
            ),
        },
        CliCommand::Startup { state } => match state {
            None => simple(Request::GetStartup, DEFAULT_TIMEOUT),
            Some(state) => simple(
                Request::SetStartup {
                    enable: matches!(state, OnOff::On),
                },
                DEFAULT_TIMEOUT,
            ),
        },
        CliCommand::Update => simple(Request::Update, UPDATE_TIMEOUT),
        CliCommand::Logs { follow, open } => logs::run(follow, open),
        CliCommand::Restart => simple(Request::Restart, RESTART_TIMEOUT),
        CliCommand::Quit => simple(Request::Quit, DEFAULT_TIMEOUT),
        CliCommand::Status { json } => status_cmd(json),
        CliCommand::Api(method) => api::run(method),
    }
}

/// Result of one request/reply exchange.
enum Outcome {
    Ok(String),
    Failed(String),
    Busy(String),
    Status(Box<StatusReply>),
    /// The server announced a relaunch (desktop self-update) and the
    /// connection ended; that is success, not a dropped connection.
    AppRestarting,
}

/// Why the control channel could not be reached.
enum ConnectError {
    /// The socket path itself is unusable (e.g. too long for `sun_path`).
    /// "Not running" would mislead here — the app may well be running with
    /// its own server disabled for the same reason.
    ///
    /// Only ever constructed on Unix: `sun_path` is a Unix domain socket
    /// limit, and the Windows `connect()` opens a fixed-name pipe that can
    /// only fail as `NotRunning`. Kept on both platforms and allowed to be
    /// dead on Windows rather than `#[cfg(unix)]`-gated.
    ///
    /// Gating was tried and does not work: it leaves `ConnectError`
    /// single-valued on Windows, so the `Err(ConnectError::NotRunning)` arms in
    /// `open_cmd` and `status_cmd` become exhaustive and the catch-all
    /// `Err(e) => connect_failed(e)` after each one is an unreachable-pattern
    /// error. Those two arms never name `BadPath`, so they don't turn up when
    /// you grep for it. A uniform enum shape keeps every match identical on
    /// both platforms and doesn't leave the same trap for the next catch-all.
    #[cfg_attr(windows, allow(dead_code))]
    BadPath(String),
    /// Connecting failed — the usual "app is not running" case.
    NotRunning,
}

/// Send a request and print the outcome; the shared path for every
/// subcommand that has no special not-running fallback.
fn simple(request: Request, timeout: Duration) -> ExitCode {
    match exchange(&request, timeout) {
        Ok(outcome) => report(outcome),
        Err(e) => connect_failed(e),
    }
}

fn connect_failed(error: ConnectError) -> ExitCode {
    match error {
        ConnectError::BadPath(message) => fail(message),
        ConnectError::NotRunning => not_running(),
    }
}

fn report(outcome: Outcome) -> ExitCode {
    match outcome {
        Outcome::Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Outcome::Failed(message) => fail(message),
        Outcome::Busy(message) => {
            eprintln!("{message}");
            ExitCode::from(EXIT_BUSY)
        }
        Outcome::AppRestarting => {
            println!("desktop update installed; the app is restarting");
            ExitCode::SUCCESS
        }
        Outcome::Status(reply) => {
            // Only `status` requests expect this reply; print it sanely anyway.
            print_status(&reply);
            ExitCode::SUCCESS
        }
    }
}

fn not_running() -> ExitCode {
    eprintln!("ESPHome Device Builder is not running.");
    eprintln!("Start it by launching the app, or run: esphome-desktop");
    ExitCode::from(EXIT_NOT_RUNNING)
}

/// Print an error and return the generic failure exit code.
fn fail(message: impl std::fmt::Display) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(EXIT_FAILED)
}

/// Connect, send one request line, and read replies until the terminal one.
/// An `Err` means the connection could not be established.
fn exchange(request: &Request, timeout: Duration) -> Result<Outcome, ConnectError> {
    let mut stream = connect()?;
    if let Err(e) = send_request(&mut stream, request) {
        return Ok(Outcome::Failed(match e {
            SendError::Encode(e) => format!("could not encode request: {e}"),
            SendError::Send(e) => format!("error sending request: {e}"),
        }));
    }
    Ok(read_replies(reply_reader(stream, timeout)))
}

/// Why [`send_request`] failed. The mapping to an error surface stays in each
/// caller ([`exchange`] formats an [`Outcome::Failed`] message for the human
/// CLI, `api_stream` emits an NDJSON error line); only the encode-and-write
/// skeleton is shared.
enum SendError {
    /// The request could not be encoded as JSON.
    Encode(serde_json::Error),
    /// The encoded request line could not be written to the stream.
    Send(std::io::Error),
}

/// Encode `request` as one newline-terminated JSON line and write it to
/// `stream`, flushing afterwards.
fn send_request<W: Write>(stream: &mut W, request: &Request) -> Result<(), SendError> {
    let mut line = serde_json::to_string(request).map_err(SendError::Encode)?;
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
        .map_err(SendError::Send)
}

/// One step of a reply read loop: what `read_line` produced, classified the
/// same way for both reply readers. The dispatch on the line's content stays
/// in each caller ([`read_replies`] maps to [`Outcome`] for the human CLI,
/// `api_read` echoes NDJSON and maps to an exit code); only this read/classify
/// skeleton is shared.
enum LineStep {
    /// A line was read into the buffer.
    Got,
    /// The server closed the connection.
    Eof,
    /// The read timed out.
    Timeout,
    /// Any other read error.
    ReadErr(std::io::Error),
}

/// Clear `line`, read the next reply line into it, and classify the result.
fn read_reply_line<R: BufRead>(reader: &mut R, line: &mut String) -> LineStep {
    line.clear();
    match reader.read_line(line) {
        Ok(0) => LineStep::Eof,
        Ok(_) => LineStep::Got,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) =>
        {
            LineStep::Timeout
        }
        Err(e) => LineStep::ReadErr(e),
    }
}

/// Read reply lines, printing progress, until a terminal reply or EOF.
fn read_replies<R: BufRead>(mut reader: R) -> Outcome {
    let mut saw_restart_marker = false;
    let mut line = String::new();
    loop {
        match read_reply_line(&mut reader, &mut line) {
            LineStep::Got => {}
            LineStep::Eof => {
                // The server closed without a terminal reply. After the
                // app-restarting marker that's expected: the relaunch tears
                // the connection down before (or while) the reply lands.
                return if saw_restart_marker {
                    Outcome::AppRestarting
                } else {
                    Outcome::Failed("connection closed before the operation finished".to_string())
                };
            }
            LineStep::Timeout => {
                return Outcome::Failed(
                    "timed out waiting for the app's reply; the operation may still be \
                     running in the app, check `esphome-desktop status`"
                        .to_string(),
                )
            }
            LineStep::ReadErr(e) => return Outcome::Failed(format!("error reading reply: {e}")),
        }
        match serde_json::from_str::<Reply>(line.trim()) {
            Ok(Reply::Progress { step, detail }) => {
                if step == STEP_APP_RESTARTING {
                    saw_restart_marker = true;
                }
                println!("  {detail}");
            }
            Ok(Reply::Ok { message }) => return Outcome::Ok(message),
            Ok(Reply::Err { message, code }) => {
                return match code {
                    ErrCode::Busy => Outcome::Busy(message),
                    ErrCode::Failed => Outcome::Failed(message),
                }
            }
            Ok(Reply::Status(reply)) => return Outcome::Status(reply),
            // No human subcommand requests a check; it only arrives via `api`,
            // which reads replies on its own path.
            Ok(Reply::UpdateCheck(_)) => {
                return Outcome::Failed("unexpected update-check reply".to_string())
            }
            Err(e) => return Outcome::Failed(format!("could not parse server reply: {e}")),
        }
    }
}

#[cfg(unix)]
fn connect() -> Result<std::os::unix::net::UnixStream, ConnectError> {
    let path = super::protocol::socket_path().map_err(|e| ConnectError::BadPath(e.to_string()))?;
    std::os::unix::net::UnixStream::connect(path).map_err(|_| ConnectError::NotRunning)
}

#[cfg(windows)]
fn connect() -> Result<std::fs::File, ConnectError> {
    // The server keeps exactly one listening pipe instance; between it
    // accepting one client and creating the next instance, opening the pipe
    // fails with ERROR_PIPE_BUSY. Retry briefly instead of misreporting
    // "not running". Byte-mode named pipes work with plain file I/O.
    const ERROR_PIPE_BUSY: i32 = 231;
    for _ in 0..40 {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(super::protocol::PIPE_NAME)
        {
            Ok(file) => return Ok(file),
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err(ConnectError::NotRunning),
        }
    }
    Err(ConnectError::NotRunning)
}

/// Wrap the connected stream in a buffered reader that enforces the per-read
/// timeout, so a stalled server-side operation frees the CLI with the
/// timed-out message on every platform.
#[cfg(unix)]
fn reply_reader(
    stream: std::os::unix::net::UnixStream,
    timeout: Duration,
) -> BufReader<std::os::unix::net::UnixStream> {
    let _ = stream.set_read_timeout(Some(timeout));
    BufReader::new(stream)
}

#[cfg(windows)]
fn reply_reader(stream: std::fs::File, timeout: Duration) -> BufReader<TimeoutReader> {
    BufReader::new(TimeoutReader::new(stream, timeout))
}

/// Per-read timeout for a pipe handle, which unlike a socket has no
/// `set_read_timeout`: a thread owns the blocking reads and forwards chunks
/// over a channel, and `read` waits on the channel with a deadline. On
/// timeout the thread is left parked on its read — harmless, because the CLI
/// exits right after reporting the timeout.
#[cfg(windows)]
struct TimeoutReader {
    chunks: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    timeout: Duration,
    buf: Vec<u8>,
    pos: usize,
}

#[cfg(windows)]
impl TimeoutReader {
    fn new(mut stream: std::fs::File, timeout: Duration) -> Self {
        let (tx, chunks) = std::sync::mpsc::channel();
        std::thread::spawn(move || loop {
            let mut chunk = vec![0u8; 4096];
            match stream.read(&mut chunk) {
                Ok(0) => {
                    // An empty chunk marks EOF (broken-pipe reads already
                    // arrive here as Ok(0) from std).
                    let _ = tx.send(Ok(Vec::new()));
                    break;
                }
                Ok(n) => {
                    chunk.truncate(n);
                    if tx.send(Ok(chunk)).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        });
        Self {
            chunks,
            timeout,
            buf: Vec::new(),
            pos: 0,
        }
    }
}

#[cfg(windows)]
impl Read for TimeoutReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.buf.len() {
            match self.chunks.recv_timeout(self.timeout) {
                Ok(Ok(chunk)) if chunk.is_empty() => return Ok(0), // EOF
                Ok(Ok(chunk)) => {
                    self.buf = chunk;
                    self.pos = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "read timed out",
                    ));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            }
        }
        let n = (self.buf.len() - self.pos).min(out.len());
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// `open`: ask the running app, or start it when it isn't running — the app
/// opens the dashboard on startup by default, so `open` still lands the user
/// in the browser.
fn open_cmd() -> ExitCode {
    match exchange(&Request::Open, DEFAULT_TIMEOUT) {
        Ok(outcome) => report(outcome),
        Err(ConnectError::NotRunning) => launch_app_detached(),
        Err(e) => connect_failed(e),
    }
}

fn launch_app_detached() -> ExitCode {
    let mut cmd = match app_launch_command() {
        Ok(cmd) => cmd,
        Err(message) => return fail(message),
    };
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from the CLI's process group so a terminal Ctrl+C after this
    // command returns doesn't take the app down with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    match cmd.spawn() {
        Ok(_child) => {
            println!(
                "ESPHome Device Builder is not running; starting it. \
                 The dashboard will open once it is ready."
            );
            ExitCode::SUCCESS
        }
        Err(e) => fail(format!("failed to start ESPHome Device Builder: {e}")),
    }
}

/// Command that starts the app the way the platform expects.
///
/// macOS must go through LaunchServices: an instance spawned directly from
/// the inner binary is not the TCC "responsible process", so the backend
/// loses the Local Network grant and mDNS discovery breaks — the same
/// failure the update relaunch path avoids (see
/// `platform::relaunch_for_update`). An AppImage must be relaunched via
/// `$APPIMAGE`: `current_exe` points inside the FUSE mount, which goes away
/// when this short-lived CLI process (the mount's owner tree) exits.
fn app_launch_command() -> Result<std::process::Command, String> {
    let exe =
        std::env::current_exe().map_err(|e| format!("could not locate the app executable: {e}"))?;

    #[cfg(target_os = "macos")]
    {
        // <bundle>.app/Contents/MacOS/<bin>; direct spawn is the dev-build
        // fallback when there is no bundle.
        if let Some(bundle) = exe
            .ancestors()
            .nth(3)
            .filter(|b| b.extension().and_then(|e| e.to_str()) == Some("app"))
        {
            let mut cmd = std::process::Command::new("/usr/bin/open");
            cmd.arg(bundle);
            return Ok(cmd);
        }
    }

    #[cfg(target_os = "linux")]
    if let Some(appimage) = super::appimage_path() {
        return Ok(std::process::Command::new(appimage));
    }

    Ok(std::process::Command::new(exe))
}

/// `status`: rich output from the running app, or a best-effort offline
/// summary read straight from settings.json when it isn't running.
fn status_cmd(json: bool) -> ExitCode {
    match exchange(&Request::Status, DEFAULT_TIMEOUT) {
        Ok(Outcome::Status(reply)) => {
            if json {
                // One stable schema for scripts: both the online and offline
                // forms carry `app_running`.
                let mut value = serde_json::to_value(reply.as_ref()).unwrap_or_default();
                if let Some(object) = value.as_object_mut() {
                    object.insert("app_running".to_string(), true.into());
                }
                println!("{value}");
            } else {
                print_status(&reply);
            }
            ExitCode::SUCCESS
        }
        Ok(other) => report(other),
        Err(ConnectError::NotRunning) => offline_status(json),
        Err(e) => connect_failed(e),
    }
}

fn print_status(status: &StatusReply) {
    println!("App:             running ({})", status.app_version);
    let backend_state = match (status.backend_running, status.backend_healthy) {
        (true, true) => "running, healthy",
        (true, false) => "running, not responding",
        (false, true) => "stopped, but something is answering on the port",
        (false, false) => "stopped",
    };
    println!("Backend:         {backend_state}");
    println!("Dashboard:       http://localhost:{}", status.port);
    println!(
        "ESPHome:         {} ({} channel)",
        status.esphome_version.as_deref().unwrap_or("unknown"),
        channel_name(status.release_channel)
    );
    println!(
        "Device builder:  {} ({} channel)",
        status
            .device_builder_version
            .as_deref()
            .unwrap_or("not installed"),
        backend_name(status.backend)
    );
    println!(
        "Launch at login: {}",
        if status.launch_at_startup {
            "on"
        } else {
            "off"
        }
    );
    println!("Config dir:      {}", status.config_dir.display());
    println!("Logs dir:        {}", status.logs_dir.display());
}

fn offline_status(json: bool) -> ExitCode {
    let data_dir = crate::platform::data_dir_no_handle();
    let settings = data_dir
        .as_ref()
        .and_then(|dir| crate::settings::peek_settings_file(&dir.join("settings.json")));

    if json {
        // Same field names and value formats as the online StatusReply form,
        // so scripts keep one stable schema whether or not the app is up;
        // only the fields knowable from settings.json are present.
        let value = match (&data_dir, &settings) {
            (Some(data_dir), Some(settings)) => {
                let config_dir = settings
                    .config_dir
                    .clone()
                    .unwrap_or_else(crate::settings::default_config_dir);
                serde_json::json!({
                    "app_running": false,
                    "port": settings.port,
                    "release_channel": settings.release_channel,
                    "backend": settings.backend,
                    "config_dir": config_dir,
                    "logs_dir": data_dir.join("logs"),
                })
            }
            _ => serde_json::json!({ "app_running": false }),
        };
        println!("{value}");
        return ExitCode::from(EXIT_NOT_RUNNING);
    }

    println!("App:             not running");
    if let (Some(data_dir), Some(settings)) = (data_dir, settings) {
        println!("Dashboard:       http://localhost:{}", settings.port);
        println!(
            "Release channel: {}",
            channel_name(settings.release_channel)
        );
        println!(
            "Backend:         {} channel",
            backend_name(settings.backend)
        );
        let config_dir = settings
            .config_dir
            .unwrap_or_else(crate::settings::default_config_dir);
        println!("Config dir:      {}", config_dir.display());
        println!("Logs dir:        {}", data_dir.join("logs").display());
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], settings.port));
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)).is_ok() {
            println!(
                "Note: something is listening on port {}; if the app was killed, \
                 its backend may still be running.",
                settings.port
            );
        }
    }
    ExitCode::from(EXIT_NOT_RUNNING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Backend, ReleaseChannel};
    use std::io::Cursor;
    use std::path::PathBuf;

    /// A reader whose next read fails with the given error, for exercising
    /// the error arms of [`read_reply_line`].
    struct ErrReader(Option<std::io::Error>);

    impl std::io::Read for ErrReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(self.0.take().expect("read called more than once"))
        }
    }

    impl BufRead for ErrReader {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            Err(self.0.take().expect("fill_buf called more than once"))
        }

        fn consume(&mut self, _amt: usize) {}
    }

    #[test]
    fn read_reply_line_classifies_all_outcomes() {
        let mut line = String::new();

        let mut got = Cursor::new(b"hello\n".to_vec());
        assert!(matches!(
            read_reply_line(&mut got, &mut line),
            LineStep::Got
        ));
        assert_eq!(line, "hello\n");

        let mut eof = Cursor::new(Vec::new());
        assert!(matches!(
            read_reply_line(&mut eof, &mut line),
            LineStep::Eof
        ));
        assert!(line.is_empty(), "buffer cleared before each read");

        for kind in [std::io::ErrorKind::TimedOut, std::io::ErrorKind::WouldBlock] {
            let mut reader = ErrReader(Some(std::io::Error::from(kind)));
            assert!(matches!(
                read_reply_line(&mut reader, &mut line),
                LineStep::Timeout
            ));
        }

        let mut broken = ErrReader(Some(std::io::Error::other("boom")));
        assert!(matches!(
            read_reply_line(&mut broken, &mut line),
            LineStep::ReadErr(_)
        ));
    }

    /// A writer whose next `write` fails with the given error (`flush`
    /// always succeeds), for exercising the write half of [`send_request`].
    struct WriteErrWriter(Option<std::io::Error>);

    impl Write for WriteErrWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(self.0.take().expect("write called more than once"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that accepts every `write` but whose next `flush` fails with
    /// the given error, for exercising the flush half of [`send_request`].
    struct FlushErrWriter(Option<std::io::Error>);

    impl Write for FlushErrWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(self.0.take().expect("flush called more than once"))
        }
    }

    #[test]
    fn send_request_writes_one_line_and_classifies_send_failures() {
        // Success: exactly the encoded request plus a trailing newline.
        let mut sink = Vec::new();
        assert!(send_request(&mut sink, &Request::Status).is_ok());
        let expected = format!("{}\n", serde_json::to_string(&Request::Status).unwrap());
        assert_eq!(sink, expected.as_bytes());

        // A write failure surfaces as SendError::Send. (Encoding a Request
        // cannot fail, so that arm has no unit test.)
        let mut broken_write = WriteErrWriter(Some(std::io::Error::other("boom")));
        assert!(matches!(
            send_request(&mut broken_write, &Request::Status),
            Err(SendError::Send(_))
        ));

        // A flush failure after a successful write also surfaces as
        // SendError::Send.
        let mut broken_flush = FlushErrWriter(Some(std::io::Error::other("boom")));
        assert!(matches!(
            send_request(&mut broken_flush, &Request::Status),
            Err(SendError::Send(_))
        ));
    }

    fn outcome_for(lines: &str) -> Outcome {
        read_replies(Cursor::new(lines.as_bytes().to_vec()))
    }

    #[test]
    fn ok_reply_after_progress() {
        let outcome = outcome_for(
            "{\"type\":\"progress\",\"step\":\"stop\",\"detail\":\"stopping\"}\n\
             {\"type\":\"ok\",\"message\":\"done\"}\n",
        );
        match outcome {
            Outcome::Ok(message) => assert_eq!(message, "done"),
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn busy_maps_to_busy_outcome() {
        let outcome = outcome_for("{\"type\":\"err\",\"message\":\"busy\",\"code\":\"busy\"}\n");
        assert!(matches!(outcome, Outcome::Busy(_)));
    }

    #[test]
    fn failed_maps_to_failed_outcome() {
        let outcome = outcome_for("{\"type\":\"err\",\"message\":\"boom\",\"code\":\"failed\"}\n");
        assert!(matches!(outcome, Outcome::Failed(_)));
    }

    #[test]
    fn eof_without_terminal_reply_is_a_failure() {
        let outcome =
            outcome_for("{\"type\":\"progress\",\"step\":\"stop\",\"detail\":\"stopping\"}\n");
        assert!(matches!(outcome, Outcome::Failed(_)));
    }

    #[test]
    fn eof_after_restart_marker_is_success() {
        // The desktop self-update relaunch closes the connection; the marker
        // tells the client that's expected.
        let outcome = outcome_for(
            "{\"type\":\"progress\",\"step\":\"app_restarting\",\"detail\":\"restarting\"}\n",
        );
        assert!(matches!(outcome, Outcome::AppRestarting));
    }

    #[test]
    fn status_reply_is_parsed() {
        let raw = serde_json::to_string(&Reply::Status(Box::new(StatusReply {
            app_version: "0.12.2".into(),
            backend_running: true,
            backend_healthy: true,
            port: 6052,
            esphome_version: None,
            device_builder_version: None,
            release_channel: ReleaseChannel::Stable,
            backend: Backend::BuilderBeta,
            launch_at_startup: false,
            config_dir: PathBuf::from("/tmp/esphome"),
            logs_dir: PathBuf::from("/tmp/logs"),
        })))
        .unwrap();
        let outcome = outcome_for(&format!("{raw}\n"));
        match outcome {
            Outcome::Status(reply) => {
                assert_eq!(reply.port, 6052);
                assert_eq!(reply.app_version, "0.12.2");
                assert!(reply.backend_running);
            }
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn garbage_reply_is_a_failure() {
        assert!(matches!(outcome_for("not json\n"), Outcome::Failed(_)));
    }

    #[cfg(unix)]
    #[test]
    fn exchange_over_a_real_unix_socket() {
        // End-to-end framing check against a canned server: request line in,
        // progress + terminal reply out.
        let dir = std::env::temp_dir().join(format!("esphome_ctl_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let sock = dir.join("test.sock");

        let listener = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("read request");
            let request: Request = serde_json::from_str(line.trim()).expect("parse request");
            assert_eq!(request, Request::Restart);
            let mut stream = stream;
            stream
                .write_all(
                    b"{\"type\":\"progress\",\"step\":\"restart\",\"detail\":\"restarting\"}\n\
                      {\"type\":\"ok\",\"message\":\"dashboard restarted and ready\"}\n",
                )
                .expect("write replies");
        });

        let mut stream = std::os::unix::net::UnixStream::connect(&sock).expect("connect");
        let mut line = serde_json::to_string(&Request::Restart).unwrap();
        line.push('\n');
        stream.write_all(line.as_bytes()).expect("send");
        let outcome = read_replies(BufReader::new(stream));
        match outcome {
            Outcome::Ok(message) => assert_eq!(message, "dashboard restarted and ready"),
            _ => panic!("expected Ok"),
        }

        server.join().expect("server thread");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
