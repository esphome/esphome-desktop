//! Detection of an external `git` executable.
//!
//! ESPHome resolves `github://` references (remote packages, external
//! components, dashboard imports) by shelling out to the system `git`
//! binary. The desktop app bundles Python but **not** git, so on a machine
//! without git installed those configs fail deep inside Python with a
//! cryptic traceback rather than an actionable message (see
//! <https://github.com/esphome/esphome-desktop/issues/113>).
//!
//! We detect the missing binary up front and surface a clear, non-blocking
//! notification so the user knows what to install and why.

use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tracing::{info, warn};

/// Candidate file names for the git executable, most-specific first.
///
/// On Windows git may be installed as `git.exe` (the usual Git-for-Windows
/// layout) or, less commonly, as a `git.cmd` shim; we accept either.
#[cfg(target_os = "windows")]
const GIT_EXECUTABLES: &[&str] = &["git.exe", "git.cmd"];

#[cfg(not(target_os = "windows"))]
const GIT_EXECUTABLES: &[&str] = &["git"];

const GIT_MISSING_TITLE: &str = "Git is not installed";

const GIT_MISSING_BODY: &str = "ESPHome needs Git to download configurations that use \
github:// packages, external components, or dashboard imports. Install Git and restart \
ESPHome Device Builder. Configs without remote references will still work.";

/// Search a PATH-style string for a git executable.
///
/// `path_var` is the raw value of the `PATH` environment variable, with
/// entries separated by the platform path separator (`:` on Unix, `;` on
/// Windows). Returns the first existing candidate found, scanning entries
/// left-to-right, or `None` if git is not present in any entry.
///
/// This is intentionally pure apart from filesystem existence checks, so it
/// can be unit-tested by passing a synthetic PATH rather than mutating the
/// process environment.
pub fn git_executable_in_path(path_var: &str) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        // Skip empty entries (e.g. a trailing separator), which would
        // otherwise resolve to the current working directory.
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in GIT_EXECUTABLES {
            let candidate = dir.join(name);
            if is_regular_file(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_regular_file(path: &Path) -> bool {
    path.is_file()
}

/// Whether a usable `git` executable can be found on the current `PATH`.
///
/// Returns `false` when the `PATH` variable is unset or contains no git
/// binary.
pub fn is_git_available() -> bool {
    match std::env::var("PATH") {
        Ok(path) => git_executable_in_path(&path).is_some(),
        Err(_) => false,
    }
}

/// Log the git-availability state and, when git is missing, show a
/// one-time notification explaining why remote (`github://`) packages will
/// fail. No-op when git is present.
pub fn notify_if_git_missing(app_handle: &AppHandle) {
    if is_git_available() {
        info!("git executable found on PATH");
        return;
    }

    warn!(
        "git executable not found on PATH; github:// packages and external \
         components will fail until git is installed (see issue #113)"
    );

    if let Err(e) = app_handle
        .notification()
        .builder()
        .title(GIT_MISSING_TITLE)
        .body(GIT_MISSING_BODY)
        .show()
    {
        warn!("Failed to show git-missing notification: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    /// Create a unique, empty temp directory for a test and return its path.
    /// Distinct per-test names keep parallel test runs from colliding.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("esphome_git_check_{tag}"));
        // Start from a clean slate in case a previous run left it behind.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn touch_git_executable(dir: &Path) -> PathBuf {
        let path = dir.join(GIT_EXECUTABLES[0]);
        fs::write(&path, b"#!/bin/sh\n").expect("write fake git");
        path
    }

    fn join_path(dirs: &[&Path]) -> String {
        std::env::join_paths(dirs.iter().map(|d| d.as_os_str()))
            .expect("join paths")
            .into_string()
            .expect("path to string")
    }

    #[test]
    fn finds_git_when_present_on_path() {
        let dir = unique_temp_dir("present");
        let git = touch_git_executable(&dir);

        let path_var = join_path(&[dir.as_path()]);
        assert_eq!(git_executable_in_path(&path_var), Some(git));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_none_when_git_absent() {
        let dir = unique_temp_dir("absent");

        let path_var = join_path(&[dir.as_path()]);
        assert_eq!(git_executable_in_path(&path_var), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_path_yields_none() {
        assert_eq!(git_executable_in_path(""), None);
    }

    #[test]
    fn scans_entries_left_to_right_and_skips_missing_dirs() {
        let empty = unique_temp_dir("scan_empty");
        let with_git = unique_temp_dir("scan_with_git");
        let git = touch_git_executable(&with_git);

        // A directory that doesn't exist, then the empty dir, then the one
        // holding git — the search must walk past the first two.
        let missing = empty.join("does-not-exist");
        let path_var = join_path(&[missing.as_path(), empty.as_path(), with_git.as_path()]);

        assert_eq!(git_executable_in_path(&path_var), Some(git));

        let _ = fs::remove_dir_all(&empty);
        let _ = fs::remove_dir_all(&with_git);
    }

    #[test]
    fn directory_named_git_is_not_treated_as_executable() {
        let dir = unique_temp_dir("dir_named_git");
        // A *directory* called `git` (or `git.exe`) must not be mistaken for
        // the executable.
        fs::create_dir_all(dir.join(GIT_EXECUTABLES[0])).expect("create git dir");

        let path_var = join_path(&[dir.as_path()]);
        assert_eq!(git_executable_in_path(&path_var), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
