//! The `api` subcommand: the machine-readable NDJSON surface the
//! device-builder dashboard codes against.
//!
//! Split out of the parent module to keep it under the file-size cap. The
//! wire plumbing it drives — `connect`, `send_request`, `reply_reader`,
//! `read_reply_line` and their error/step enums — stays in the parent and is
//! reached via `super::`, so the human CLI and this surface share one
//! transport.

use std::io::{BufRead, Write};
use std::process::ExitCode;
use std::time::Duration;

use super::{
    connect, read_reply_line, reply_reader, send_request, ConnectError, LineStep, SendError,
    DEFAULT_TIMEOUT, EXIT_BUSY, EXIT_FAILED, EXIT_NOT_RUNNING, UPDATE_TIMEOUT,
};
use crate::control::protocol::{self, ErrCode, Reply, Request, STEP_APP_RESTARTING};
use crate::ApiMethod;

/// The operation succeeded.
const EXIT_SUCCESS: u8 = 0;

/// `check-update` hits GitHub and PyPI and spawns Python for the installed
/// versions; more headroom than a local request, far less than an install.
const CHECK_TIMEOUT: Duration = Duration::from_secs(120);

/// `api <method>`: the machine-readable contract the device-builder dashboard
/// codes against. Emits newline-delimited JSON only — one object per line, on
/// stdout, valid JSON even for errors — so the human CLI's wording stays free
/// to change. Versioned via [`protocol::API_SCHEMA_VERSION`].
pub(super) fn run(method: ApiMethod) -> ExitCode {
    let (request, timeout) = match method {
        // Pure handshake: answer even while the app is starting (or absent), so
        // the dashboard can gate on the version before making any other call.
        ApiMethod::Version => {
            println!(
                "{}",
                serde_json::json!({ "schema_version": protocol::API_SCHEMA_VERSION })
            );
            return ExitCode::SUCCESS;
        }
        ApiMethod::Status => (Request::Status, DEFAULT_TIMEOUT),
        ApiMethod::CheckUpdate => (Request::CheckUpdate, CHECK_TIMEOUT),
        // Non-interactive by construction: the server restarts the backend
        // without any consent, so an unattended remote builder recovers itself.
        ApiMethod::Update => (Request::Update, UPDATE_TIMEOUT),
    };
    // The stream helpers work in raw exit codes so they stay unit-testable
    // (ExitCode is opaque); wrap once here at the boundary.
    ExitCode::from(api_stream(&request, timeout))
}

/// Connect, send one request, and forward each server reply as a validated JSON
/// line (trimmed of surrounding whitespace) until the terminal reply or EOF. The
/// exit code mirrors the terminal reply for shell callers; the JSON line is
/// authoritative for the dashboard.
fn api_stream(request: &Request, timeout: Duration) -> u8 {
    let mut stream = match connect() {
        Ok(stream) => stream,
        Err(ConnectError::NotRunning) => {
            return api_err_line("not_running", "the app is not running", EXIT_NOT_RUNNING)
        }
        Err(ConnectError::BadPath(message)) => {
            return api_err_line("bad_path", &message, EXIT_FAILED)
        }
    };
    if let Err(e) = send_request(&mut stream, request) {
        return match e {
            SendError::Encode(e) => api_err_line("encode_failed", &e.to_string(), EXIT_FAILED),
            SendError::Send(e) => api_err_line("send_failed", &e.to_string(), EXIT_FAILED),
        };
    }
    api_read(reply_reader(stream, timeout))
}

/// Drain reply lines, forwarding each validated JSON line, until a terminal reply.
/// Returns the exit code the terminal reply maps to.
fn api_read<R: BufRead>(mut reader: R) -> u8 {
    let mut saw_restart_marker = false;
    let mut line = String::new();
    loop {
        match read_reply_line(&mut reader, &mut line) {
            LineStep::Got => {}
            LineStep::Eof => {
                // A desktop self-update relaunch tears the connection down right
                // after its terminal `ok`; the marker tells us that's success.
                return if saw_restart_marker {
                    EXIT_SUCCESS
                } else {
                    api_err_line(
                        "connection_closed",
                        "connection closed before the operation finished",
                        EXIT_FAILED,
                    )
                };
            }
            LineStep::Timeout => {
                return api_err_line(
                    "timeout",
                    "timed out waiting for the app's reply; the operation may still be running",
                    EXIT_FAILED,
                )
            }
            LineStep::ReadErr(e) => {
                return api_err_line("read_failed", &e.to_string(), EXIT_FAILED)
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Parse before echoing: the `api` contract is NDJSON-only, so a line
        // that is not valid JSON must never reach stdout (it would break a
        // dashboard doing `json.loads` per line). Guard that case up front, then
        // echo exactly once for every valid-JSON line.
        let reply = serde_json::from_str::<Reply>(trimmed);
        if reply.is_err() && serde_json::from_str::<serde_json::Value>(trimmed).is_err() {
            return api_err_line(
                "protocol_error",
                "the app sent a line that was not valid JSON",
                EXIT_FAILED,
            );
        }
        echo_json_line(trimmed);
        match reply {
            Ok(Reply::Ok { .. }) | Ok(Reply::Status(_)) | Ok(Reply::UpdateCheck(_)) => {
                return EXIT_SUCCESS
            }
            Ok(Reply::Err { code, .. }) => {
                return match code {
                    ErrCode::Busy => EXIT_BUSY,
                    ErrCode::Failed => EXIT_FAILED,
                }
            }
            Ok(Reply::Progress { step, .. }) => {
                if step == STEP_APP_RESTARTING {
                    saw_restart_marker = true;
                }
            }
            // Valid JSON, just not a Reply we recognize: already echoed (still
            // NDJSON); keep reading for the terminal reply. This is defensive —
            // the client and app are the same binary, so their reply shapes
            // always match — but it keeps a stray line from ending the stream.
            Err(_) => {}
        }
    }
}

/// Print one already-validated JSON line and flush, so a dashboard consuming
/// this process's stdout as a pipe sees each progress line promptly rather than
/// only when the OS buffer fills.
fn echo_json_line(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

/// Print a synthesized client-side error as one JSON line and return `exit`.
/// Client-only `code`s (`not_running`, `timeout`, ...) sit alongside the
/// server's `busy`/`failed`; the dashboard treats `code` as an opaque string.
fn api_err_line(code: &str, message: &str, exit: u8) -> u8 {
    println!(
        "{}",
        serde_json::json!({ "type": "err", "code": code, "message": message })
    );
    exit
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn api_exit_for(lines: &str) -> u8 {
        api_read(Cursor::new(lines.as_bytes().to_vec()))
    }

    #[test]
    fn api_ok_after_progress_exits_success() {
        let exit = api_exit_for(
            "{\"type\":\"progress\",\"step\":\"esphome\",\"detail\":\"installing\"}\n\
             {\"type\":\"ok\",\"message\":\"done\"}\n",
        );
        assert_eq!(exit, EXIT_SUCCESS);
    }

    #[test]
    fn api_update_check_reply_exits_success() {
        let raw = serde_json::to_string(&Reply::UpdateCheck(Box::new(
            crate::control::protocol::UpdateCheckReply {
                any_available: false,
                app: crate::control::protocol::ComponentUpdate::not_installed(),
                esphome: crate::control::protocol::ComponentUpdate::not_installed(),
                device_builder: crate::control::protocol::ComponentUpdate::not_installed(),
            },
        )))
        .unwrap();
        assert_eq!(api_exit_for(&format!("{raw}\n")), EXIT_SUCCESS);
    }

    #[test]
    fn api_busy_and_failed_map_to_their_exit_codes() {
        assert_eq!(
            api_exit_for("{\"type\":\"err\",\"message\":\"busy\",\"code\":\"busy\"}\n"),
            EXIT_BUSY
        );
        assert_eq!(
            api_exit_for("{\"type\":\"err\",\"message\":\"boom\",\"code\":\"failed\"}\n"),
            EXIT_FAILED
        );
    }

    #[test]
    fn api_eof_after_restart_marker_is_success() {
        // The load-bearing case for unattended builders: the self-update
        // relaunch closes the connection after the marker, which is success.
        let exit = api_exit_for(
            "{\"type\":\"progress\",\"step\":\"app_restarting\",\"detail\":\"restarting\"}\n",
        );
        assert_eq!(exit, EXIT_SUCCESS);
    }

    #[test]
    fn api_eof_without_terminal_reply_is_a_failure() {
        let exit =
            api_exit_for("{\"type\":\"progress\",\"step\":\"stop\",\"detail\":\"stopping\"}\n");
        assert_eq!(exit, EXIT_FAILED);
    }

    #[test]
    fn api_non_json_line_is_a_protocol_error() {
        // The `api` contract is NDJSON-only: a line that isn't valid JSON must
        // not be echoed as-is (it would break a per-line `json.loads`); it ends
        // the stream with a synthesized JSON error instead.
        let exit = api_exit_for("not json\n{\"type\":\"ok\",\"message\":\"done\"}\n");
        assert_eq!(exit, EXIT_FAILED);
    }

    #[test]
    fn api_unknown_json_variant_is_echoed_and_stream_continues() {
        // A line that is valid JSON but not a Reply we recognize (e.g. a newer
        // server variant) is still NDJSON, so it's forwarded and the following
        // terminal reply still decides the exit code.
        let exit = api_exit_for("{\"type\":\"future\"}\n{\"type\":\"ok\",\"message\":\"done\"}\n");
        assert_eq!(exit, EXIT_SUCCESS);
    }
}
