//! CLI side of the control channel (`esphome-desktop <subcommand>`).
//!
//! Runs without Tauri and without a tokio runtime: plain std sockets are all
//! a one-shot request/reply exchange needs. `logs` never touches the channel
//! at all — the log paths are deterministic from the bundle identifier.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use super::protocol::{
    backend_name, channel_name, ErrCode, Reply, Request, StatusReply, STEP_APP_RESTARTING,
};
use crate::{CliCommand, OnOff};

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

/// Lines shown by the default (non-follow) `logs` tail.
const TAIL_LINES: usize = 50;
/// How far back from the end of the file the tail looks.
const TAIL_WINDOW_BYTES: u64 = 64 * 1024;

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
        CliCommand::Logs { follow, open } => logs_cmd(follow, open),
        CliCommand::Restart => simple(Request::Restart, RESTART_TIMEOUT),
        CliCommand::Quit => simple(Request::Quit, DEFAULT_TIMEOUT),
        CliCommand::Status { json } => status_cmd(json),
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
    let mut line = match serde_json::to_string(request) {
        Ok(line) => line,
        Err(e) => return Ok(Outcome::Failed(format!("could not encode request: {e}"))),
    };
    line.push('\n');
    if let Err(e) = stream
        .write_all(line.as_bytes())
        .and_then(|()| stream.flush())
    {
        return Ok(Outcome::Failed(format!("error sending request: {e}")));
    }
    Ok(read_replies(reply_reader(stream, timeout)))
}

/// Read reply lines, printing progress, until a terminal reply or EOF.
fn read_replies<R: BufRead>(mut reader: R) -> Outcome {
    let mut saw_restart_marker = false;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // The server closed without a terminal reply. After the
                // app-restarting marker that's expected: the relaunch tears
                // the connection down before (or while) the reply lands.
                return if saw_restart_marker {
                    Outcome::AppRestarting
                } else {
                    Outcome::Failed("connection closed before the operation finished".to_string())
                };
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Outcome::Failed(
                    "timed out waiting for the app's reply; the operation may still be \
                     running in the app, check `esphome-desktop status`"
                        .to_string(),
                )
            }
            Err(e) => return Outcome::Failed(format!("error reading reply: {e}")),
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
    {
        if let Some(appimage) = std::env::var_os("APPIMAGE") {
            if !appimage.is_empty() {
                return Ok(std::process::Command::new(appimage));
            }
        }
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
    if json {
        println!("{}", serde_json::json!({ "app_running": false }));
        return ExitCode::from(EXIT_NOT_RUNNING);
    }
    println!("App:             not running");
    if let Some(data_dir) = crate::platform::data_dir_no_handle() {
        if let Some(settings) = crate::settings::peek_settings_file(&data_dir.join("settings.json"))
        {
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
    }
    ExitCode::from(EXIT_NOT_RUNNING)
}

/// `logs`: print/tail the dashboard log, or open the logs folder. Fully
/// offline — works whether or not the app is running.
fn logs_cmd(follow: bool, open_dir: bool) -> ExitCode {
    let Some(logs_dir) = crate::platform::data_dir_no_handle().map(|d| d.join("logs")) else {
        return fail("could not resolve the logs directory");
    };
    if open_dir {
        return match open::that_detached(&logs_dir) {
            Ok(()) => {
                println!("opened {}", logs_dir.display());
                ExitCode::SUCCESS
            }
            Err(e) => fail(format!("failed to open {}: {e}", logs_dir.display())),
        };
    }

    let log_path = logs_dir.join(crate::daemon::DASHBOARD_LOG_NAME);
    println!("Dashboard log: {}", log_path.display());
    println!();
    let pos = match print_tail(&log_path) {
        Ok(pos) => pos,
        Err(e) => {
            if !follow {
                return fail(format!("could not read {}: {e}", log_path.display()));
            }
            println!("(waiting for {} to appear)", log_path.display());
            0
        }
    };
    if !follow {
        return ExitCode::SUCCESS;
    }
    follow_log(&log_path, pos)
}

/// Print the last [`TAIL_LINES`] lines of the file and return the offset the
/// follow loop should continue from (the end of the file at read time).
fn print_tail(path: &Path) -> std::io::Result<u64> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    for line in tail_lines(&text, TAIL_LINES, start > 0) {
        println!("{line}");
    }
    // Continue from what was actually printed, not the pre-read length —
    // bytes appended during the read would otherwise print twice.
    Ok(start + buf.len() as u64)
}

/// Last `n` lines of `text`. With `truncated`, the first line is dropped:
/// the read started mid-file, so it is almost certainly partial.
fn tail_lines(text: &str, n: usize, truncated: bool) -> Vec<&str> {
    let mut lines: Vec<&str> = text.lines().collect();
    if truncated && !lines.is_empty() {
        lines.remove(0);
    }
    let skip = lines.len().saturating_sub(n);
    lines.split_off(skip)
}

/// Follow the log by polling for growth. The daemon rotates dashboard.log on
/// every backend start, so a rotation is detected by file identity where the
/// platform exposes one — a shrunk length alone misses the case where the
/// fresh file outgrows the old offset within one poll, which would silently
/// skip its head — with the length check as the fallback.
fn follow_log(path: &Path, mut pos: u64) -> ExitCode {
    let mut identity = std::fs::metadata(path)
        .ok()
        .as_ref()
        .and_then(file_identity);
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let Ok(meta) = std::fs::metadata(path) else {
            pos = 0;
            identity = None;
            continue;
        };
        let current = file_identity(&meta);
        if (current.is_some() && current != identity) || meta.len() < pos {
            if pos > 0 {
                println!("--- log rotated ---");
            }
            pos = 0;
        }
        identity = current;
        if meta.len() == pos {
            continue;
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        if file.seek(SeekFrom::Start(pos)).is_err() {
            continue;
        }
        let mut buf = Vec::new();
        if file.read_to_end(&mut buf).is_err() {
            continue;
        }
        pos += buf.len() as u64;
        print!("{}", String::from_utf8_lossy(&buf));
        let _ = std::io::stdout().flush();
    }
}

/// Stable identity of the file behind the metadata, used to detect rotation.
#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

/// Windows has no cheap inode equivalent here; the length heuristic remains.
#[cfg(windows)]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Backend, ReleaseChannel};
    use std::io::Cursor;
    use std::path::PathBuf;

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

    #[test]
    fn tail_lines_keeps_only_the_last_n() {
        let text = "a\nb\nc\nd\ne\n";
        assert_eq!(tail_lines(text, 3, false), vec!["c", "d", "e"]);
        assert_eq!(tail_lines(text, 10, false), vec!["a", "b", "c", "d", "e"]);
    }

    #[test]
    fn tail_lines_drops_partial_first_line_when_truncated() {
        // A mid-file read starts inside a line; the fragment must not be shown.
        let text = "tial line\nb\nc\n";
        assert_eq!(tail_lines(text, 10, true), vec!["b", "c"]);
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
