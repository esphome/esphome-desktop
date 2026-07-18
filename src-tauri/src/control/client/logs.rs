//! The `logs` subcommand: tail and follow the dashboard log.
//!
//! This never touches the control channel — the log paths are deterministic
//! from the bundle identifier, so it works even when the app is not running.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use super::fail;

/// Lines shown by the default (non-follow) `logs` tail.
const TAIL_LINES: usize = 50;
/// How far back from the end of the file the tail looks.
const TAIL_WINDOW_BYTES: u64 = 64 * 1024;

pub(super) fn run(follow: bool, open_dir: bool) -> ExitCode {
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
}
