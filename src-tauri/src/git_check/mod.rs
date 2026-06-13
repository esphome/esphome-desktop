//! Detection of an external `git` executable.
//!
//! ESPHome shells out to the system `git` binary for a range of common
//! features: external components, remote (`github://`) packages, dashboard
//! imports, micro_wake_word voice models, and ESP-IDF managed components. The
//! desktop app bundles Python but **not** git, so on a machine without git
//! installed those features fail deep inside Python with a cryptic traceback
//! rather than an actionable message (see
//! <https://github.com/esphome/esphome-desktop/issues/113>).
//!
//! We detect the missing binary up front and surface a clear, non-blocking
//! notification so the user knows what to install and why.

use std::ffi::OsStr;
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

const GIT_MISSING_BODY: &str = "ESPHome uses Git to download external components, remote \
(github://) packages, voice models, and other dependencies, so many configurations won't \
compile without it. Install Git, then restart ESPHome Device Builder so it can detect it.";

const PARENT_GIT_REPO_TITLE: &str = "A parent folder is a Git repository";

/// Search a PATH-style value for a git executable.
///
/// `path_var` is the raw value of the `PATH` environment variable, with
/// entries separated by the platform path separator (`:` on Unix, `;` on
/// Windows). It is an `OsStr` rather than a `str` so a non-Unicode `PATH`
/// (legal on both Unix and Windows) is handled instead of being treated as
/// "git missing". Returns the first existing candidate found, scanning entries
/// left-to-right, or `None` if git is not present in any entry.
///
/// This is intentionally pure apart from filesystem existence checks, so it
/// can be unit-tested by passing a synthetic PATH rather than mutating the
/// process environment.
pub fn git_executable_in_path(path_var: &OsStr) -> Option<PathBuf> {
    for dir in std::env::split_paths(path_var) {
        // Skip empty entries (e.g. a trailing separator), which would
        // otherwise resolve to the current working directory.
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in GIT_EXECUTABLES {
            let candidate = dir.join(name);
            if is_git_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `path` is a usable git executable.
///
/// `is_file()` is false for a directory, so a directory named `git` (or
/// `git.exe`) is correctly rejected. On Unix we additionally require an
/// execute bit: a non-executable regular file named `git` on `PATH` is not a
/// binary ESPHome could actually run, so it shouldn't suppress the warning. On
/// Windows the file extension (`git.exe` / `git.cmd`) is the executability
/// signal, so presence is enough.
fn is_git_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Whether a usable `git` executable can be found on the current `PATH`.
///
/// Returns `false` when the `PATH` variable is unset or contains no git
/// binary.
fn is_git_available() -> bool {
    // `var_os` (not `var`) so a non-Unicode PATH doesn't read as "git missing".
    match std::env::var_os("PATH") {
        Some(path) => git_executable_in_path(&path).is_some(),
        None => false,
    }
}

/// Log the git-availability state and, when git is missing, show a
/// notification explaining why remote (`github://`) packages will fail.
///
/// Called once per launch, after the daemon starts successfully. While git
/// stays missing the notification reappears on each launch (by design — the
/// whole point is surfacing the cause); installing git silences it. No-op when
/// git is present.
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

/// Find a `.git` entry in a *strict ancestor* of `config_dir`, returning the
/// repository root (the ancestor directory that contains `.git`).
///
/// ESP-IDF's CMake Git-revision detection
/// (`GetGitRevisionDescription.cmake`) walks **upward** from the build tree
/// looking for the nearest `.git`. If it finds one belonging to an unrelated
/// repository — e.g. `C:\` or the user's home folder accidentally `git init`ed
/// — the ESP-IDF package, which has no `.git` of its own, fails the build with
/// an opaque `head-ref` file error rather than anything actionable (see
/// <https://github.com/esphome/esphome-desktop/issues/170>).
///
/// We deliberately skip `config_dir` itself: version-controlling your ESPHome
/// configuration directory is a legitimate, encouraged workflow, so a `.git`
/// *at* the config dir must not warn. Only a `.git` in a parent or higher —
/// which the user almost never intends to scope over their ESPHome builds — is
/// flagged. `.git` may be a directory (normal repo) or a file (worktree /
/// submodule pointer), so existence rather than directory-ness is the test.
///
/// Pure apart from filesystem existence checks, so it is unit-testable with a
/// synthetic directory tree.
fn git_repo_in_ancestors(config_dir: &Path) -> Option<PathBuf> {
    // `ancestors()` yields the path itself first; `skip(1)` excludes the
    // config dir so a version-controlled config folder doesn't trip the check.
    for ancestor in config_dir.ancestors().skip(1) {
        if ancestor.join(".git").exists() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

/// Warn (non-blocking) when the ESPHome config directory sits inside an
/// unrelated Git repository, which can make ESP-IDF builds fail with an opaque
/// CMake `head-ref` error (issue #170).
///
/// Called once per launch, after the daemon starts successfully — same cadence
/// as [`notify_if_git_missing`]. No-op when no ancestor repository is found.
pub fn notify_if_config_dir_in_git_repo(app_handle: &AppHandle, config_dir: &Path) {
    let Some(repo_root) = git_repo_in_ancestors(config_dir) else {
        return;
    };

    warn!(
        "config directory {} is inside a Git repository rooted at {}; ESP-IDF \
         builds may fail with an opaque CMake head-ref error (see issue #170)",
        config_dir.display(),
        repo_root.display()
    );

    let body = format!(
        "A parent folder ({}) is a Git repository. ESP-IDF builds can pick up \
         that repository and fail to compile with an opaque CMake \"head-ref\" \
         error. If your devices fail to build, remove the stray .git folder \
         from that parent directory, or move your ESPHome configuration outside \
         the repository.",
        repo_root.display()
    );

    if let Err(e) = app_handle
        .notification()
        .builder()
        .title(PARENT_GIT_REPO_TITLE)
        .body(body)
        .show()
    {
        warn!("Failed to show parent-git-repo notification: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Create a unique, empty temp directory for a test and return its path.
    /// The process id plus a per-test tag keeps both intra-process parallelism
    /// and two concurrent `cargo test` binaries on the same host from
    /// colliding.
    fn unique_temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("esphome_git_check_{}_{tag}", std::process::id()));
        // Start from a clean slate in case a previous run left it behind.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn touch_git_executable(dir: &Path) -> PathBuf {
        let path = dir.join(GIT_EXECUTABLES[0]);
        fs::write(&path, b"#!/bin/sh\n").expect("write fake git");
        // On Unix the detector requires an execute bit, so a real-looking git
        // must be marked executable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                .expect("chmod +x fake git");
        }
        path
    }

    fn join_path(dirs: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(dirs.iter().map(|d| d.as_os_str())).expect("join paths")
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
        assert_eq!(git_executable_in_path(OsStr::new("")), None);
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

    fn mkdir(path: &Path) {
        fs::create_dir_all(path).expect("create dir");
    }

    #[test]
    fn no_git_in_any_ancestor_yields_none() {
        let root = unique_temp_dir("anc_none");
        let config = root.join("a").join("b").join("esphome");
        mkdir(&config);

        assert_eq!(git_repo_in_ancestors(&config), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_in_parent_is_detected() {
        let root = unique_temp_dir("anc_parent");
        let parent = root.join("workspace");
        let config = parent.join("esphome");
        mkdir(&config);
        mkdir(&parent.join(".git"));

        assert_eq!(git_repo_in_ancestors(&config), Some(parent));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_in_grandparent_is_detected() {
        let root = unique_temp_dir("anc_grandparent");
        let grandparent = root.join("repo");
        let config = grandparent.join("nested").join("esphome");
        mkdir(&config);
        mkdir(&grandparent.join(".git"));

        assert_eq!(git_repo_in_ancestors(&config), Some(grandparent));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_at_config_dir_itself_is_ignored() {
        // Version-controlling the ESPHome config directory is a legitimate
        // workflow, so a `.git` *at* the config dir must not be flagged.
        let root = unique_temp_dir("anc_self");
        let config = root.join("esphome");
        mkdir(&config);
        mkdir(&config.join(".git"));

        assert_eq!(git_repo_in_ancestors(&config), None);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn nearest_ancestor_repo_wins() {
        // When both a parent and a grandparent are repositories, the nearest
        // (the parent) is reported.
        let root = unique_temp_dir("anc_nearest");
        let grandparent = root.join("outer");
        let parent = grandparent.join("inner");
        let config = parent.join("esphome");
        mkdir(&config);
        mkdir(&grandparent.join(".git"));
        mkdir(&parent.join(".git"));

        assert_eq!(git_repo_in_ancestors(&config), Some(parent));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_as_a_file_is_detected() {
        // A `.git` *file* (worktree / submodule pointer) counts as a repo.
        let root = unique_temp_dir("anc_gitfile");
        let parent = root.join("worktree");
        let config = parent.join("esphome");
        mkdir(&config);
        fs::write(parent.join(".git"), b"gitdir: /elsewhere\n").expect("write .git file");

        assert_eq!(git_repo_in_ancestors(&config), Some(parent));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn non_executable_git_file_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("non_exec");
        // A regular file named `git` without an execute bit is not a usable
        // binary, so it must not be treated as git being present.
        let path = dir.join(GIT_EXECUTABLES[0]);
        fs::write(&path, b"not a binary\n").expect("write file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod -x");

        let path_var = join_path(&[dir.as_path()]);
        assert_eq!(git_executable_in_path(&path_var), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
