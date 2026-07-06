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

use crate::i18n::{t, t_with};

/// Candidate file names for the git executable, most-specific first.
///
/// On Windows git may be installed as `git.exe` (the usual Git-for-Windows
/// layout) or, less commonly, as a `git.cmd` shim; we accept either.
#[cfg(target_os = "windows")]
const GIT_EXECUTABLES: &[&str] = &["git.exe", "git.cmd"];

#[cfg(not(target_os = "windows"))]
const GIT_EXECUTABLES: &[&str] = &["git"];

/// Iterate over every git executable found on a PATH-style value, in
/// left-to-right order.
///
/// `path_var` is the raw value of the `PATH` environment variable, with
/// entries separated by the platform path separator (`:` on Unix, `;` on
/// Windows). It is an `OsStr` rather than a `str` so a non-Unicode `PATH`
/// (legal on both Unix and Windows) is handled instead of being treated as
/// "git missing".
///
/// All candidates are yielded (rather than stopping at the first match) so
/// callers can keep scanning past an unusable one — e.g. the macOS
/// `/usr/bin/git` stub shadowing a later real git.
///
/// This is intentionally pure apart from filesystem existence checks, so it
/// can be unit-tested by passing a synthetic PATH rather than mutating the
/// process environment.
fn git_executables_in_path(path_var: &OsStr) -> impl Iterator<Item = PathBuf> + '_ {
    std::env::split_paths(path_var)
        // Skip empty entries (e.g. a trailing separator), which would
        // otherwise resolve to the current working directory.
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(|dir| GIT_EXECUTABLES.iter().map(move |name| dir.join(name)))
        .filter(|candidate| is_git_executable(candidate))
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
    // Scan every git on PATH and accept the first *usable* one, so a macOS
    // `/usr/bin/git` stub earlier in PATH doesn't mask a real git later in it.
    match std::env::var_os("PATH") {
        Some(path) => git_executables_in_path(&path).any(|p| git_is_usable(&p)),
        None => false,
    }
}

/// Whether a git executable found on `PATH` is actually runnable.
///
/// On most platforms, presence on `PATH` (with an execute bit, checked by
/// [`is_git_executable`]) is sufficient. macOS is the exception: a Mac without
/// the Xcode Command Line Tools still ships `/usr/bin/git` as a **stub** whose
/// only job is to pop the CLT installer when invoked. That stub is a real,
/// executable file, so it satisfies the PATH + execute-bit checks and makes
/// the PATH scan report git as present even though no working git
/// exists. ESPHome would then resolve the same stub and fail deep inside
/// Python, exactly the cryptic failure this module exists to prevent.
///
/// `xcode-select -p` exits 0 only when the Command Line Tools are actually
/// installed and is side-effect free, so we use it to tell a real git from the
/// stub. We only second-guess `/usr/bin/git`; any other path (Homebrew, a real
/// CLT git resolved elsewhere) is taken at face value.
fn git_is_usable(found: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        macos_git_usable(found, command_line_tools_installed)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = found; // the stub only exists on macOS; elsewhere PATH presence is enough.
        true
    }
}

/// Pure decision for [`git_is_usable`] on macOS: a `/usr/bin/git` is real only
/// when the Command Line Tools are installed; any other path is taken as-is.
/// Split out from the `xcode-select` probe so the stub logic is unit-testable.
///
/// `clt_installed` is taken lazily so the `xcode-select` probe only runs for the
/// `/usr/bin/git` stub — a non-stub path (e.g. Homebrew) never shells out.
#[cfg(target_os = "macos")]
fn macos_git_usable(found: &Path, clt_installed: impl FnOnce() -> bool) -> bool {
    found != Path::new("/usr/bin/git") || clt_installed()
}

/// Whether the macOS Command Line Tools are installed.
///
/// `xcode-select -p` prints the active developer directory and exits 0 only
/// when the tools are present; it exits non-zero (and is otherwise a no-op)
/// when they are absent, so it is a safe, side-effect-free probe for telling a
/// working git from the `/usr/bin/git` installer stub.
#[cfg(target_os = "macos")]
fn command_line_tools_installed() -> bool {
    use std::process::Command;

    Command::new("/usr/bin/xcode-select")
        .arg("-p")
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Show the git-missing notification with the given body.
fn show_git_missing_notification(app_handle: &AppHandle, body: String) {
    if let Err(e) = app_handle
        .notification()
        .builder()
        .title(t("git_check.missing_title"))
        .body(body)
        .show()
    {
        warn!("Failed to show git-missing notification: {}", e);
    }
}

/// Trigger Apple's Command Line Tools installer, which bundles git.
///
/// `xcode-select --install` opens the native macOS install dialog. It needs no
/// admin rights and returns immediately (the install proceeds in that dialog),
/// and it's a no-op that exits non-zero when the tools are already installed or
/// an install is already running — so it's safe to call whenever git is
/// missing. We re-trigger on each launch while git stays absent, matching the
/// notification's cadence; once the tools land, git is found and neither fires.
#[cfg(target_os = "macos")]
fn trigger_command_line_tools_install() {
    use std::process::Command;

    match Command::new("/usr/bin/xcode-select")
        .arg("--install")
        .output()
    {
        Ok(out) if out.status.success() => {
            info!("Opened the Command Line Tools installer (xcode-select --install)");
        }
        // Non-zero is expected when the tools are already installed or an
        // install is already in progress; log it but don't treat it as fatal.
        Ok(out) => info!(
            "xcode-select --install exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => warn!("Failed to run xcode-select --install: {}", e),
    }
}

/// Log the git-availability state and, when git is missing, surface it.
///
/// On macOS we additionally trigger the Command Line Tools installer (which
/// includes git) so the user gets Apple's native install dialog instead of
/// having to find and run the git-scm.com installer themselves; elsewhere we
/// just show the notification.
///
/// Called once per launch, after the daemon starts successfully. While git
/// stays missing the notification (and, on macOS, the installer prompt)
/// reappears on each launch (by design — the whole point is surfacing the
/// cause); installing git silences it. No-op when git is present.
pub fn notify_if_git_missing(app_handle: &AppHandle) {
    if is_git_available() {
        info!("git executable found on PATH");
        return;
    }

    warn!(
        "git executable not found on PATH; github:// packages and external \
         components will fail until git is installed (see issue #113)"
    );

    // macOS has its own body: we trigger Apple's Command Line Tools installer
    // (which includes git) via `xcode-select --install`, so point the user at
    // that dialog rather than the daunting git-scm.com download.
    #[cfg(target_os = "macos")]
    {
        trigger_command_line_tools_install();
        show_git_missing_notification(app_handle, t("git_check.missing_body_macos"));
    }

    #[cfg(not(target_os = "macos"))]
    {
        show_git_missing_notification(app_handle, t("git_check.missing_body"));
    }
}

/// True when `dir` directly contains a `.git` entry — a directory (a normal
/// repo) or a file (a worktree / submodule pointer). Existence, not
/// directory-ness, is the test.
fn has_git_entry(dir: &Path) -> bool {
    dir.join(".git").exists()
}

/// Resolve the parent Git repository above `config_dir` that could break
/// ESP-IDF builds, or `None` when there is nothing to flag. `is_repo` reports
/// whether a directory is itself a repo — production passes [`has_git_entry`];
/// tests pass a fake so the resolution stays unit-testable without touching the
/// filesystem.
///
/// ESP-IDF's CMake Git-revision detection (`GetGitRevisionDescription.cmake`)
/// walks **upward** from the *build* tree looking for the nearest `.git` (see
/// <https://github.com/esphome/esphome-desktop/issues/170>). Where that build
/// tree sits differs by platform, so the scope of the check does too — per
/// @bdraco's review ("on windows this should check the drive root, and on
/// linux/mac it should keep the tree check"):
///
/// - **Windows**: the build directory is `C:\esphb\<id8>` (nested under a
///   shared `C:\esphb`, set by esphome-device-builder's own
///   `windows_short_build_paths` helper, never by this app). `C:\esphb` is
///   app-created/owned (no stray `.git`), and the only ancestor it shares with
///   the config dir is the drive root `C:\`. So `C:\.git` is the one stray repo
///   that actually breaks a build. Checking every config-dir ancestor would
///   over-warn on legitimate layouts that never feed the build — configs kept
///   inside a project repo (`C:\Users\me\dev\esphome` + `C:\Users\me\dev\.git`)
///   or a `~\.git` dotfiles repo — so **only the drive root** is checked.
///
///   Caveat: `windows_short_build_paths` is a no-op when `ESPHOME_DATA_DIR` is
///   preset (or dashboard-id / relocation setup fails), in which case the build
///   tree falls back to `<config>/.esphome`, under the config dir like Unix.
///   The drive-root-only check then misses an intermediate breaker; this is a
///   known, narrow under-warn gap on that fallback path.
/// - **Unix (Linux/macOS)**: the build tree lives under the config directory,
///   so any enclosing repo can feed CMake's upward walk. We therefore keep the
///   full **tree check** — every strict ancestor, nearest wins. The config dir
///   itself is skipped: version-controlling your ESPHome configs is legitimate
///   and must not warn.
///
/// A **relative** `config_dir` (the `PathBuf::from("esphome")` `home_dir()`
/// fallback, or a relative user-supplied `settings.config_dir`) is first
/// absolutized against the current working directory so the ancestor chain
/// terminates at a real root rather than an empty path.
fn find_parent_git_repo(config_dir: &Path, is_repo: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    // Absolutize a relative config dir so the ancestor walk terminates at a
    // real filesystem root rather than an empty path.
    let absolute;
    let config_dir = if config_dir.is_absolute() {
        config_dir
    } else {
        absolute = std::env::current_dir().ok()?.join(config_dir);
        &absolute
    };

    #[cfg(target_os = "windows")]
    {
        // Only the drive root can feed the build dir's (`C:\esphb\<id8>`)
        // upward CMake walk. `ancestors()` yields the path itself first and the drive
        // root last. The config dir is never flagged when it *is* the root.
        let root = config_dir.ancestors().last()?;
        if root.as_os_str().is_empty() || root == config_dir {
            return None;
        }
        is_repo(root).then(|| root.to_path_buf())
    }

    #[cfg(not(target_os = "windows"))]
    {
        // Walk strict ancestors (skip the config dir itself), nearest first;
        // the build tree sits under the config dir, so any enclosing repo is a
        // candidate.
        config_dir
            .ancestors()
            .skip(1)
            .filter(|a| !a.as_os_str().is_empty())
            .find(|a| is_repo(a))
            .map(Path::to_path_buf)
    }
}

/// Warn (non-blocking) when a stray Git repository sits above the ESPHome
/// config directory, which can make ESP-IDF builds fail with an opaque CMake
/// `head-ref` error (issue #170).
///
/// The scope is platform-specific — the Windows drive root, the full ancestor
/// tree on Linux/macOS (per @bdraco's review). See [`find_parent_git_repo`].
///
/// Called once per launch, after the daemon starts successfully — same cadence
/// as [`notify_if_git_missing`]. No-op when no parent repository is found.
pub fn notify_if_config_dir_in_git_repo(app_handle: &AppHandle, config_dir: &Path) {
    let Some(repo_root) = find_parent_git_repo(config_dir, has_git_entry) else {
        return;
    };

    warn!(
        "config directory {} sits inside a Git repository rooted at {}; \
         ESP-IDF builds may fail with an opaque CMake head-ref error (see issue \
         #170)",
        config_dir.display(),
        repo_root.display()
    );

    let body = t_with(
        "git_check.parent_repo_body",
        &[
            ("config_dir", &config_dir.display().to_string()),
            ("repo_root", &repo_root.display().to_string()),
        ],
    );

    if let Err(e) = app_handle
        .notification()
        .builder()
        .title(t("git_check.parent_repo_title"))
        .body(body)
        .show()
    {
        warn!("Failed to show parent-git-repo notification: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;
    use std::fs;
    use std::path::{Path, PathBuf};

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
        assert_eq!(git_executables_in_path(&path_var).next(), Some(git));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_none_when_git_absent() {
        let dir = unique_temp_dir("absent");

        let path_var = join_path(&[dir.as_path()]);
        assert_eq!(git_executables_in_path(&path_var).next(), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_path_yields_none() {
        assert_eq!(git_executables_in_path(OsStr::new("")).next(), None);
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

        assert_eq!(git_executables_in_path(&path_var).next(), Some(git));

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
        assert_eq!(git_executables_in_path(&path_var).next(), None);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_normal_git_path_is_usable() {
        // Any path other than the macOS `/usr/bin/git` stub is taken at face
        // value on every platform (on non-macOS, all paths are).
        assert!(git_is_usable(Path::new("/opt/homebrew/bin/git")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_usr_bin_git_needs_command_line_tools() {
        // The `/usr/bin/git` stub is only a real git once the CLT are installed.
        assert!(!macos_git_usable(Path::new("/usr/bin/git"), || false));
        assert!(macos_git_usable(Path::new("/usr/bin/git"), || true));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_non_stub_git_is_usable_without_command_line_tools() {
        // A git found anywhere else (e.g. Homebrew) is never the stub, so the
        // CLT probe is never consulted (this closure must not run).
        assert!(macos_git_usable(Path::new("/opt/homebrew/bin/git"), || {
            panic!("CLT probe must not run for a non-stub path")
        }));
    }

    fn mkdir(path: &Path) {
        fs::create_dir_all(path).expect("create dir");
    }

    #[test]
    fn no_git_entry_in_dir_is_false() {
        let root = unique_temp_dir("entry_none");
        let config = root.join("esphome");
        mkdir(&config);

        assert!(!has_git_entry(&config));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_directory_is_an_entry() {
        let root = unique_temp_dir("entry_dir");
        mkdir(&root.join(".git"));

        assert!(has_git_entry(&root));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn git_file_is_an_entry() {
        // A `.git` *file* (worktree / submodule pointer) counts as a repo.
        let root = unique_temp_dir("entry_file");
        fs::write(root.join(".git"), b"gitdir: /elsewhere\n").expect("write .git file");

        assert!(has_git_entry(&root));

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_nearest_ancestor_repo_wins() {
        // Tree check (Linux/macOS): the nearest enclosing repo is reported.
        let config = Path::new("/home/me/dev/esphome");
        let near = Path::new("/home/me/dev");
        let is_repo = |p: &Path| p == near || p == Path::new("/home");

        assert_eq!(
            find_parent_git_repo(config, is_repo),
            Some(near.to_path_buf())
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_config_dir_itself_is_not_flagged() {
        // Version-controlling the config dir is legitimate; only ancestors count.
        let config = Path::new("/home/me/dev/esphome");
        let is_repo = |p: &Path| p == config;

        assert_eq!(find_parent_git_repo(config, is_repo), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_no_ancestor_repo_yields_none() {
        let config = Path::new("/home/me/dev/esphome");

        assert_eq!(find_parent_git_repo(config, |_| false), None);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn unix_config_dir_at_root_yields_none() {
        // A config dir that is itself the filesystem root has no parent to flag.
        let fs_root = Path::new("/home/me").ancestors().last().unwrap();

        assert_eq!(find_parent_git_repo(fs_root, |_| true), None);
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_checks_only_the_drive_root() {
        // On Windows the build dir's only ancestor is the drive root, so an
        // intermediate repo must NOT be flagged...
        let config = Path::new(r"C:\Users\me\dev\esphome");
        let intermediate = |p: &Path| p == Path::new(r"C:\Users\me\dev");
        assert_eq!(find_parent_git_repo(config, intermediate), None);

        // ...only a repo at the drive root is.
        let drive_root = config.ancestors().last().unwrap().to_path_buf();
        let at_root = {
            let drive_root = drive_root.clone();
            move |p: &Path| p == drive_root
        };
        assert_eq!(find_parent_git_repo(config, at_root), Some(drive_root));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_config_dir_at_drive_root_yields_none() {
        // A config dir that is itself the drive root has no parent to flag.
        let drive_root = Path::new(r"C:\Users\me").ancestors().last().unwrap();

        assert_eq!(find_parent_git_repo(drive_root, |_| true), None);
    }

    #[test]
    fn relative_config_dir_absolutizes_against_cwd() {
        // The `home_dir()` fallback (`PathBuf::from("esphome")`) and relative
        // user settings are absolutized against cwd, so the ancestor walk
        // terminates at a real filesystem root rather than an empty path. The
        // filesystem/drive root is a strict ancestor on every platform (and the
        // sole checked one on Windows), so flagging it works cross-platform.
        let cwd_root = std::env::current_dir()
            .expect("cwd")
            .ancestors()
            .last()
            .unwrap()
            .to_path_buf();

        let root = find_parent_git_repo(Path::new("esphome"), |p| p == cwd_root);

        assert_eq!(root, Some(cwd_root));
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
        assert_eq!(git_executables_in_path(&path_var).next(), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
