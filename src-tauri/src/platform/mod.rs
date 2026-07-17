//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tracing::debug;

mod base_manifest;
mod health;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod process;
mod python_env;

pub use base_manifest::{can_refresh_from_bundle, python_path_for_reset, wipe_installed_packages};
pub use health::{
    clear_repair_count, esphome_config_probe, is_managed_python_tree, may_repair_tree,
    repair_budget_left,
};
#[cfg(target_os = "windows")]
pub use process::{assign_to_kill_on_close_job, send_ctrl_break};
pub use process::{
    configure_daemon_tokio_command, isolate_python_tokio_command, pip_command, run_python_capture,
    run_python_capture_stdout,
};
pub use python_env::{ensure_user_python, interpreter_is_usable, RefreshReason};

/// Application bundle identifier. Must match the `identifier` field in
/// `tauri.conf.json`; Tauri derives `app_data_dir()` from it, and code that
/// resolves the data dir before an `AppHandle` exists joins it manually.
pub const BUNDLE_IDENTIFIER: &str = "io.esphome.builder";

/// Get the application data directory
///
/// - macOS: `~/Library/Application Support/io.esphome.builder/`
/// - Windows: `%APPDATA%\io.esphome.builder\`
/// - Linux: `~/.local/share/io.esphome.builder/`
pub fn get_data_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let path = app_handle
        .path()
        .app_data_dir()
        .context("Failed to get app data directory")?;

    // Ensure directory exists
    std::fs::create_dir_all(&path).context("Failed to create data directory")?;

    debug!("Data directory: {:?}", path);
    Ok(path)
}

/// Resolve the app data directory without an `AppHandle`.
///
/// The CLI client mode (`esphome-desktop <subcommand>`) runs without a Tauri
/// app, so it cannot use `app_data_dir()`. Joining the bundle identifier onto
/// the OS data dir is the same derivation Tauri uses, and the same one
/// `app_log_appender` in `lib.rs` already relies on. Does not create the
/// directory.
pub fn data_dir_no_handle() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(BUNDLE_IDENTIFIER))
}

/// Get the bundled resource directory.
///
/// On Linux we resolve this ourselves so the path is always
/// `<prefix>/lib/esphome-desktop/` — no spaces, no dependence on
/// Tauri's `resource_dir()` which uses the product name ("ESPHome Device Builder").
///
/// The sharun-based AppImage format patches `/usr/…` paths in the binary
/// with random `/tmp/…` tokens, so `std::env::current_exe()` returns a
/// path like `/tmp/.mount_XXX/tmp/<rand>/esphome-desktop` instead of the
/// real `<mount>/bin/esphome-desktop`.  We therefore prefer the `APPDIR`
/// env var that sharun always sets, and fall back to exe-relative
/// resolution for deb/AUR installs where `APPDIR` is absent.
///
/// On macOS and Windows, Tauri's `resource_dir()` works correctly.
fn get_bundled_resource_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        // 1. Prefer APPDIR (set by sharun-based AppImage at runtime)
        if let Ok(appdir) = std::env::var("APPDIR") {
            let resource_dir = PathBuf::from(&appdir).join("lib/esphome-desktop");
            if resource_dir.is_dir() {
                debug!("Bundled resource dir (APPDIR): {:?}", resource_dir);
                return Ok(resource_dir);
            }
            debug!(
                "APPDIR set to {:?} but {:?} does not exist",
                appdir, resource_dir
            );
        }

        // 2. Resolve relative to the real executable (deb/AUR installs)
        let exe = std::env::current_exe().context("Failed to get current executable path")?;
        let exe_dir = exe.parent().context("Failed to get executable directory")?;
        // bin/esphome-desktop -> ../lib/esphome-desktop/
        let resource_dir = exe_dir.join("../lib/esphome-desktop");
        if let Ok(resolved) = resource_dir.canonicalize() {
            debug!("Bundled resource dir (resolved): {:?}", resolved);
            return Ok(resolved);
        }

        // 3. Fallback to Tauri's resource_dir for development builds
        let fallback = app_handle
            .path()
            .resource_dir()
            .context("Failed to get resource directory")?;
        debug!("Bundled resource dir (fallback): {:?}", fallback);
        Ok(fallback)
    }

    #[cfg(not(target_os = "linux"))]
    {
        let resource_dir = app_handle
            .path()
            .resource_dir()
            .context("Failed to get resource directory")?;
        debug!("Bundled resource dir: {:?}", resource_dir);
        Ok(resource_dir)
    }
}

/// Get the path to the user Python executable
/// On non-Windows platforms, the bundled Python is copied to user data for updates
pub fn get_python_path(app_handle: &AppHandle) -> Result<PathBuf> {
    let data_dir = get_data_dir(app_handle)?;
    let python_path = health::interpreter_in_tree(&data_dir.join("python"));

    if python_path.exists() {
        debug!("Using user Python: {:?}", python_path);
        return Ok(python_path);
    }

    // Fall back to bundled Python (will be copied on first run)
    let resource_dir = get_bundled_resource_dir(app_handle)?;
    let bundled_python = health::interpreter_in_tree(&resource_dir.join("python"));

    if bundled_python.exists() {
        debug!("Using bundled Python: {:?}", bundled_python);
        return Ok(bundled_python);
    }

    // Fall back to system Python (for development)
    debug!("Falling back to system Python");
    Ok(PathBuf::from(if cfg!(target_os = "windows") {
        "python"
    } else {
        "python3"
    }))
}

/// Get the Python bin directory (for PATH)
pub fn get_python_bin(app_handle: &AppHandle) -> Result<PathBuf> {
    let data_dir = get_data_dir(app_handle)?;
    let user_python = data_dir.join("python");

    #[cfg(target_os = "windows")]
    let bin_dir = user_python.clone(); // On Windows, python.exe is in the root

    #[cfg(not(target_os = "windows"))]
    let bin_dir = user_python.join("bin");

    // If user Python exists, use it
    if bin_dir.exists() {
        return Ok(bin_dir);
    }

    // Fall back to bundled Python
    let resource_dir = get_bundled_resource_dir(app_handle)?;

    #[cfg(target_os = "windows")]
    let bundled_bin = resource_dir.join("python"); // On Windows, python.exe is in the root

    #[cfg(not(target_os = "windows"))]
    let bundled_bin = resource_dir.join("python").join("bin");

    Ok(bundled_bin)
}

/// Directory inside the bundled `git` resource that holds `git.exe`.
///
/// MinGit lays out a `cmd/git.exe` wrapper (alongside `mingw64/bin/git.exe`);
/// `cmd` is the directory Git-for-Windows recommends putting on `PATH`.
#[cfg(target_os = "windows")]
pub fn get_bundled_git_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let resource_dir = get_bundled_resource_dir(app_handle)?;
    Ok(resource_dir.join("git").join("cmd"))
}

/// Directory inside the bundled `git` resource that holds a GNU `patch.exe`.
///
/// MinGit ships no `patch`, but the esphome micro-opus ESP-IDF build needs one
/// on `PATH` to patch the Opus source (issue #189). `prepare_bundle.sh` harvests
/// `patch.exe` (and the MSYS DLLs it links) from PortableGit into `git/patch/`.
/// We expose only this dir, not MinGit's full `usr/bin`, so the build doesn't
/// pick up MSYS `sh`/`find`/`sort` that shadow Windows built-ins.
#[cfg(target_os = "windows")]
pub fn get_bundled_patch_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let resource_dir = get_bundled_resource_dir(app_handle)?;
    Ok(resource_dir.join("git").join("patch"))
}

/// Directory inside the bundled `ccache` resource that holds `ccache.exe`.
///
/// `prepare_bundle.sh` extracts a single static `ccache.exe` into `ccache/`.
/// Putting this dir on `PATH` lets ESPHome's ESP-IDF build discover ccache and
/// enable compiler caching automatically.
#[cfg(target_os = "windows")]
pub fn get_bundled_ccache_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let resource_dir = get_bundled_resource_dir(app_handle)?;
    Ok(resource_dir.join("ccache"))
}

/// Build a `PATH` value with `dir` prepended to `existing`.
///
/// Pure (no environment mutation) so the prepend ordering, separator
/// correctness, and non-Unicode `PATH` preservation can be unit-tested with a
/// synthetic value rather than touching the real process environment — the same
/// split-the-logic pattern `git_check::git_executables_in_path` uses. Going
/// through `split_paths`/`join_paths` keeps the platform separator correct and
/// round-trips a non-Unicode `PATH` instead of lossily dropping it.
// Reached outside tests only through insert_dir_into_path, whose callers are
// Windows (bundled tools) and macOS (Homebrew).
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn path_with_prepended(existing: &OsStr, dir: &Path) -> Result<OsString> {
    // An empty `existing` (PATH unset) would split into a single empty entry,
    // leaving a trailing "" in the result — which Windows search semantics
    // treat as the current directory. Return just `dir` in that case.
    if existing.is_empty() {
        return Ok(dir.as_os_str().to_os_string());
    }
    let mut entries = vec![dir.to_path_buf()];
    entries.extend(std::env::split_paths(existing));
    std::env::join_paths(entries).context("Failed to build PATH with bundled git prepended")
}

/// Build a `PATH` value with `dir` appended after `existing`.
///
/// The append counterpart of [`path_with_prepended`], pure for the same reason
/// (split/join keeps the platform separator correct and round-trips a
/// non-Unicode `PATH`). Used to expose Homebrew at the *end* of `PATH` so a
/// brew-installed tool (e.g. `ccache`) is discoverable without ever shadowing a
/// system or bundled binary that resolves earlier (see [`ensure_homebrew_on_path`]).
// Reached outside tests only through insert_dir_into_path; see path_with_prepended.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn path_with_appended(existing: &OsStr, dir: &Path) -> Result<OsString> {
    // An empty `existing` (PATH unset) would split into a single empty entry,
    // leaving a leading "" in the result — which Windows search semantics treat
    // as the current directory. Return just `dir` in that case.
    if existing.is_empty() {
        return Ok(dir.as_os_str().to_os_string());
    }
    let mut entries: Vec<PathBuf> = std::env::split_paths(existing).collect();
    entries.push(dir.to_path_buf());
    std::env::join_paths(entries).context("Failed to build PATH with Homebrew appended")
}

/// Where to insert a directory into `PATH`.
// No caller constructs either variant on Linux.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Clone, Copy)]
enum PathInsert {
    /// Prepend, so the dir shadows anything already on `PATH`. For bundled tools
    /// we always want to win (MinGit, the bundled ccache).
    // Constructed only by prepend_bundled_tool, which is Windows only.
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    Front,
    /// Append, so the dir is only a fallback and never shadows an earlier entry.
    /// For the Homebrew dirs on macOS.
    // Constructed only by ensure_homebrew_on_path's macOS body.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Back,
}

/// Insert `dir` into this process's `PATH` — the single place that mutates the
/// environment, so the spawned daemon (which inherits our environment) and any
/// later `PATH` probe both observe it. Returns `true` if `PATH` changed.
///
/// Idempotent for both positions: a `dir` already on `PATH` is left in place and
/// returns `false`. This keeps the mutation safe to call more than once in a
/// process (re-init flows, tests) without growing `PATH` unboundedly toward the
/// Windows environment-size limit. Routed through
/// [`path_with_prepended`]/[`path_with_appended`] so the platform separator and
/// a non-Unicode `PATH` are handled correctly.
// Both callers are cfg gated: prepend_bundled_tool (Windows) and
// ensure_homebrew_on_path's macOS body. Dead on Linux, deliberately compiled
// everywhere so all three lint gates see the same code.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn insert_dir_into_path(dir: &Path, position: PathInsert) -> Result<bool> {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    if std::env::split_paths(&existing).any(|p| p == dir) {
        return Ok(false);
    }
    let new_path = match position {
        PathInsert::Front => path_with_prepended(&existing, dir)?,
        PathInsert::Back => path_with_appended(&existing, dir)?,
    };
    std::env::set_var("PATH", &new_path);
    Ok(true)
}

/// Put a bundled tool's directory at the front of this process's `PATH`
/// (Windows only).
///
/// If `dir` contains `exe_name`, ensures `dir` is at the front of `PATH`
/// (prepending it unless it is already present, per [`insert_dir_into_path`]),
/// logs it, and returns `true`; `true` means the tool exists and its directory
/// is on `PATH`, not that `PATH` was necessarily modified. If the exe is
/// missing, warns with `missing_consequence` and returns `false` without
/// touching `PATH`, leaving the caller to decide whether to bail out or
/// continue.
#[cfg(target_os = "windows")]
fn prepend_bundled_tool(
    dir: &Path,
    exe_name: &str,
    human_name: &str,
    missing_consequence: &str,
) -> Result<bool> {
    use tracing::{info, warn};

    let exe = dir.join(exe_name);
    if !exe.exists() {
        warn!(
            "Bundled {} missing at {:?}; {}",
            human_name, exe, missing_consequence
        );
        return Ok(false);
    }
    insert_dir_into_path(dir, PathInsert::Front)?;
    info!("Using bundled {} at {:?}", human_name, exe);
    Ok(true)
}

/// Ensure a usable `git` is on `PATH` for the ESPHome backend we spawn.
///
/// ESPHome / PlatformIO / esphome-device-builder shell out to `git` for
/// external components, `github://` packages, voice models, ESP-IDF managed
/// components, and `git+https://` deps. Windows ships no git, so we bundle
/// MinGit (which covers every git feature these use: HTTPS clone + submodules)
/// and make it discoverable here (see issue #160).
///
/// Windows only: prepend the bundled MinGit `cmd` directory to this process's
/// `PATH`. The spawned daemon inherits the process environment (it never sets
/// `PATH` itself), and `git_check::notify_if_git_missing` reads the same
/// `PATH`, so this single mutation both lets ESPHome find git and silences the
/// missing-git notification. We always use the bundled git rather than probing
/// for a system one — MinGit does everything we need, so there's no reason to
/// add the complexity of preferring (and validating) whatever git a user
/// happens to have.
///
/// No-op on macOS (the Command Line Tools prompt covers a missing git) and
/// Linux (git ships on all but the most minimal installs).
pub fn ensure_git_on_path(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let git_dir = get_bundled_git_dir(app_handle)?;
        if !prepend_bundled_tool(
            &git_dir,
            "git.exe",
            "MinGit",
            "git-dependent features will fail until git is on PATH",
        )? {
            return Ok(());
        }

        // Also expose the bundled GNU patch (issue #189) when present. Prepended
        // after git so it too sits ahead of the inherited PATH; only this
        // dedicated dir goes on PATH, not MinGit's full usr/bin, so the build
        // doesn't pick up MSYS sh/find/sort that shadow Windows built-ins.
        // A missing patch.exe is log-and-continue: git alone is still useful.
        let patch_dir = get_bundled_patch_dir(app_handle)?;
        prepend_bundled_tool(
            &patch_dir,
            "patch.exe",
            "patch",
            "micro-opus and other components that need `patch` will fail to build",
        )?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app_handle;
    }

    Ok(())
}

/// Append Homebrew's bin directories to this process's `PATH` (macOS only).
///
/// The ESPHome backend we spawn inherits this process's environment verbatim
/// (it never sets `PATH` itself), and the app normally launches as a login item,
/// so it gets the sparse GUI session `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin`
/// plus whatever `path_helper` adds) — which excludes Homebrew. ESP-IDF builds
/// pick up `ccache` automatically when it's on `PATH`, so making a
/// `brew install ccache` discoverable here lets those builds use it.
///
/// We append (not prepend) `/opt/homebrew/bin` (Apple Silicon) and
/// `/usr/local/bin` (Intel) so a system or bundled binary that resolves earlier
/// is never shadowed by a Homebrew copy — Homebrew is only a fallback for tools
/// the base `PATH` doesn't provide. Each dir is added only if it exists and is
/// not already on `PATH`, keeping the value clean (`path_helper` may already
/// list `/usr/local/bin`).
///
/// No-op on non-macOS. `app_handle` is accepted for signature symmetry with
/// [`ensure_git_on_path`] (and so the call site reads the same).
pub fn ensure_homebrew_on_path(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        use tracing::info;

        let _ = app_handle;

        // Apple Silicon first, then Intel; both are appended when present so a
        // single build artifact works on either architecture. `insert_dir_into_path`
        // skips a dir already on PATH (path_helper may list `/usr/local/bin`).
        for brew_bin in ["/opt/homebrew/bin", "/usr/local/bin"] {
            let brew_dir = Path::new(brew_bin);
            if brew_dir.is_dir() && insert_dir_into_path(brew_dir, PathInsert::Back)? {
                info!("Appended Homebrew dir {:?} to PATH", brew_dir);
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app_handle;
    }

    Ok(())
}

/// Ensure the bundled `ccache` is on `PATH` for the ESPHome backend we spawn.
///
/// ESPHome's ESP-IDF build turns on compiler caching automatically when a
/// `ccache` binary is found on `PATH`, roughly halving repeat-build times.
/// Windows ships no ccache and users rarely install one, so we bundle the
/// official static build (`prepare_bundle.sh`) and prepend its directory here.
/// The spawned daemon inherits this process's environment (it never sets `PATH`
/// itself), so this single mutation is enough for the build to see ccache.
///
/// No-op on macOS (a brew-installed ccache is reached via the Homebrew dirs
/// appended in `ensure_homebrew_on_path`) and Linux (ccache is a distro
/// package). Log-and-continue if the bundled exe is missing: builds just run
/// without caching, exactly as before.
pub fn ensure_ccache_on_path(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // There is no system ccache on Windows to shadow, so prepend vs append
        // is immaterial; prepend keeps it consistent with the bundled git/patch
        // handling above.
        let ccache_dir = get_bundled_ccache_dir(app_handle)?;
        prepend_bundled_tool(
            &ccache_dir,
            "ccache.exe",
            "ccache",
            "ESP-IDF builds will run without compiler caching",
        )?;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app_handle;
    }

    Ok(())
}

/// Platform-specific initialization
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
pub fn init(app_handle: &AppHandle) {
    #[cfg(target_os = "macos")]
    macos::init(app_handle);

    #[cfg(target_os = "windows")]
    windows::init();

    #[cfg(target_os = "linux")]
    linux::init();
}

/// Relaunch the app after a desktop update.
///
/// On macOS this goes through LaunchServices (`open`) instead of Tauri's
/// [`tauri::AppHandle::restart`], which respawns the inner Mach-O directly. A
/// directly respawned instance is not the LaunchServices-launched, TCC
/// "responsible" process, so the bundled Python backend it spawns is not covered
/// by the app's Local Network grant: its mDNS multicast then fails with
/// `EHOSTUNREACH` ("No route to host") until the user manually relaunches.
/// Reopening via LaunchServices gives the new instance the correct
/// responsibility, so device discovery works immediately after an update. Other
/// platforms keep `restart()` (Local Network privacy is macOS-only).
pub fn relaunch_for_update(app_handle: &AppHandle) {
    #[cfg(target_os = "macos")]
    if macos::spawn_launchservices_relaunch() {
        // The watcher reopens us once we're gone; exit cleanly so it can.
        app_handle.exit(0);
        return;
    }
    // Non-macOS, or the LaunchServices path couldn't be set up: fall back to
    // Tauri's direct relaunch (diverges).
    app_handle.restart();
}

#[cfg(target_os = "windows")]
mod windows {
    pub fn init() {
        // Windows-specific initialization
    }
}

/// One-shot cleanup of the legacy `/Applications/ESPHome Builder.app` bundle
/// left behind when the desktop app was renamed to "ESPHome Device Builder".
///
/// On the first launch after the rename the user is prompted (via a native
/// dialog) to move the old bundle to the Trash. The decision is recorded as
/// a marker file in the app data directory so the prompt is not repeated.
///
/// User settings and the bundled Python tree live under the bundle
/// identifier (`io.esphome.builder/`), which did not change with the rename,
/// so no data migration is needed.
pub fn cleanup_legacy_macos_app(app_handle: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
        use tracing::{info, warn};

        const OLD_APP: &str = "/Applications/ESPHome Builder.app";
        const MARKER_NAME: &str = ".legacy_macos_app_cleanup";

        if !PathBuf::from(OLD_APP).exists() {
            return;
        }

        let data_dir = match get_data_dir(app_handle) {
            Ok(d) => d,
            Err(e) => {
                debug!("Skipping legacy app cleanup; data dir unavailable: {}", e);
                return;
            }
        };

        let marker = data_dir.join(MARKER_NAME);
        if marker.exists() {
            return;
        }

        info!("Legacy {} detected; prompting user to remove it", OLD_APP);

        let dialog_app = app_handle.clone();
        std::thread::spawn(move || {
            let confirmed = dialog_app
                .dialog()
                .message(crate::i18n::t("platform.remove_legacy_prompt"))
                .title(crate::i18n::t("platform.remove_legacy_title"))
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    crate::i18n::t("platform.move_to_trash"),
                    crate::i18n::t("platform.keep"),
                ))
                .blocking_show();

            if confirmed {
                let script = format!(
                    "tell application \"Finder\" to delete POSIX file \"{}\"",
                    OLD_APP
                );
                match std::process::Command::new("osascript")
                    .args(["-e", &script])
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        info!("Moved {} to Trash", OLD_APP);
                    }
                    Ok(out) => {
                        warn!(
                            "Failed to move {} to Trash: {}",
                            OLD_APP,
                            String::from_utf8_lossy(&out.stderr).trim()
                        );
                    }
                    Err(e) => warn!("Failed to spawn osascript: {}", e),
                }
            }

            // Marker is written regardless so the user is not nagged.
            if let Err(e) = std::fs::write(&marker, "") {
                warn!("Failed to write legacy-cleanup marker: {}", e);
            }
        });
    }

    #[cfg(not(target_os = "macos"))]
    {
        let _ = app_handle;
    }
}

/// Returns `true` on Linux when the appindicator library required for system
/// tray support is available, and always `true` on non-Linux platforms (which
/// use native APIs that don't require a separate shared library).
pub fn is_tray_supported() -> bool {
    #[cfg(target_os = "linux")]
    {
        linux::is_appindicator_available()
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::base_manifest::BASE_MANIFEST;
    use super::health::{interpreter_in_tree, python_tree_root};
    use super::*;
    use crate::util::unique_temp_dir;

    #[test]
    fn path_with_prepended_puts_dir_first() {
        let existing = std::env::join_paths(["/usr/bin", "/bin"]).unwrap();
        let joined = path_with_prepended(&existing, Path::new("/opt/git/cmd")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/opt/git/cmd"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
            ],
            "bundled git dir must come first so it shadows anything already on PATH"
        );
    }

    #[test]
    fn path_with_prepended_chains_two_bundled_dirs() {
        // ensure_git_on_path prepends git/cmd then git/patch (#189). Both bundled
        // dirs must end up ahead of the inherited PATH.
        let existing = std::env::join_paths(["/usr/bin"]).unwrap();
        let with_git = path_with_prepended(&existing, Path::new("/opt/git/cmd")).unwrap();
        let with_patch = path_with_prepended(&with_git, Path::new("/opt/git/patch")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&with_patch).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/opt/git/patch"),
                PathBuf::from("/opt/git/cmd"),
                PathBuf::from("/usr/bin"),
            ],
        );
    }

    #[test]
    fn path_with_prepended_onto_empty_yields_just_dir() {
        // var_os("PATH") missing degrades to an empty value; the result must be
        // exactly the bundled git dir with no trailing empty entry (an empty
        // PATH entry means the current directory under Windows search rules).
        let joined = path_with_prepended(OsStr::new(""), Path::new("/opt/git/cmd")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(entries, vec![PathBuf::from("/opt/git/cmd")]);
    }

    /// A non-Unicode `PATH` is legal on Unix; the prepend must round-trip its
    /// bytes verbatim rather than lossily mangling them (the whole reason the
    /// helper works in `OsStr`/`OsString` instead of `str`).
    #[cfg(unix)]
    #[test]
    fn path_with_prepended_preserves_non_unicode_existing() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        // 0xFF is not valid UTF-8 and is not the path separator, so it survives
        // both the join and a re-split.
        let existing = OsString::from_vec(b"/weird\xffdir".to_vec());
        let joined = path_with_prepended(&existing, Path::new("/opt/git/cmd")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(entries[0], PathBuf::from("/opt/git/cmd"));
        assert_eq!(entries[1].as_os_str().as_bytes(), b"/weird\xffdir");
    }

    #[test]
    fn path_with_appended_puts_dir_last() {
        let existing = std::env::join_paths(["/usr/bin", "/bin"]).unwrap();
        let joined = path_with_appended(&existing, Path::new("/opt/homebrew/bin")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                PathBuf::from("/opt/homebrew/bin"),
            ],
            "Homebrew dir must come last so it never shadows anything already on PATH"
        );
    }

    #[test]
    fn path_with_appended_chains_two_dirs_in_order() {
        // ensure_homebrew_on_path appends /opt/homebrew/bin then /usr/local/bin.
        // Both must land after the inherited PATH, in append order.
        let existing = std::env::join_paths(["/usr/bin"]).unwrap();
        let with_arm = path_with_appended(&existing, Path::new("/opt/homebrew/bin")).unwrap();
        let with_intel = path_with_appended(&with_arm, Path::new("/usr/local/bin")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&with_intel).collect();
        assert_eq!(
            entries,
            vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ],
        );
    }

    #[test]
    fn path_with_appended_onto_empty_yields_just_dir() {
        // var_os("PATH") missing degrades to an empty value; the result must be
        // exactly the appended dir with no leading empty entry (an empty PATH
        // entry means the current directory under Windows search rules).
        let joined = path_with_appended(OsStr::new(""), Path::new("/opt/homebrew/bin")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(entries, vec![PathBuf::from("/opt/homebrew/bin")]);
    }

    /// A non-Unicode `PATH` is legal on Unix; the append must round-trip its
    /// bytes verbatim, exactly like the prepend counterpart.
    #[cfg(unix)]
    #[test]
    fn path_with_appended_preserves_non_unicode_existing() {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let existing = OsString::from_vec(b"/weird\xffdir".to_vec());
        let joined = path_with_appended(&existing, Path::new("/opt/homebrew/bin")).unwrap();
        let entries: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(entries[0].as_os_str().as_bytes(), b"/weird\xffdir");
        assert_eq!(entries[1], PathBuf::from("/opt/homebrew/bin"));
    }

    /// Env var naming the real bundled Python tree the e2e test runs against.
    const E2E_TREE_ENV: &str = "ESPHOME_E2E_PYTHON_TREE";

    /// Run the tree's interpreter and return (success, stdout+stderr).
    ///
    /// Goes through [`run_python_capture`] so the harness is isolated exactly as
    /// the code under test is. Spawning the interpreter directly would let the
    /// runner's user site-packages or an ambient `PYTHONPATH` satisfy an import
    /// (#318) — and this test asserts on *absence* ("esphome survived the
    /// wipe"), which is precisely what a stray import would invert.
    fn e2e_run(python: &Path, args: &[&str]) -> (bool, String) {
        let output = run_python_capture(python, args)
            .unwrap_or_else(|e| panic!("failed to run {python:?} {args:?}: {e}"));
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        (output.status.success(), combined)
    }

    /// The tree's site-packages, straight from its own sysconfig.
    fn e2e_purelib(python: &Path) -> PathBuf {
        let (ok, out) = e2e_run(
            python,
            &[
                "-c",
                "import sysconfig; print(sysconfig.get_path('purelib'))",
            ],
        );
        assert!(ok, "could not resolve purelib: {out}");
        PathBuf::from(out.trim())
    }

    /// The whole #330 lifecycle against a real bundled Python tree: detect the
    /// orphan, wipe, prove pip survived, reinstall, prove it is fixed.
    ///
    /// Ignored by default because it needs the genuine article — the
    /// python-build-standalone tree with esphome in it that
    /// `build-scripts/prepare_bundle.sh` produces — plus network to reinstall.
    /// The `Python tree repair (e2e)` workflow builds that tree on every OS we
    /// ship and runs this with `--ignored`.
    ///
    /// A venv would not do: on Windows a venv puts `python.exe` in `Scripts/`,
    /// while the real bundle has it at the tree root, so `python_tree_root`'s
    /// platform assumption would go untested against the layout we actually
    /// ship. This uses the shipped layout.
    ///
    /// One test rather than several because each step depends on the last
    /// leaving the tree in a particular state, and Rust does not order tests.
    #[test]
    #[ignore = "needs a real bundled Python tree; run by the python-tree-repair CI job"]
    fn e2e_repair_cycle() {
        let root = PathBuf::from(std::env::var(E2E_TREE_ENV).unwrap_or_else(|_| {
            panic!("{E2E_TREE_ENV} must point at a tree built by prepare_bundle.sh")
        }));
        // One spelling of the shipped layout, shared with the unit tests: a
        // second copy here could drift from it, and this is the test that would
        // have to catch that drift.
        let python = interpreter_in_tree(&root);
        assert!(python.is_file(), "no interpreter at {python:?}");

        // The tree the build produces must resolve back to itself, or the reset
        // would aim at the wrong directory on this platform.
        assert_eq!(
            python_tree_root(&python),
            Some(root.as_path()),
            "python_tree_root disagrees with the shipped layout"
        );
        assert!(
            root.join(BASE_MANIFEST).is_file(),
            "prepare_bundle.sh must ship {BASE_MANIFEST}"
        );

        let purelib = e2e_purelib(&python);

        // 1. A freshly built tree is healthy.
        assert_eq!(
            esphome_config_probe(&python).expect("probe could not run"),
            None,
            "a freshly built bundle must pass the health probe"
        );
        let (ok, version) = e2e_run(&python, &["-m", "esphome", "version"]);
        assert!(ok, "esphome version failed: {version}");
        let version = version
            .trim()
            .rsplit(' ')
            .next()
            .expect("esphome version printed nothing")
            .to_string();

        // 2. Orphan a component directory exactly the way --ignore-installed
        //    did: `rp2` declares `rp2040` as a legacy alias, so a leftover
        //    `rp2040` package from the previous version collides with it.
        let orphan = purelib.join("esphome").join("components").join("rp2040");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("__init__.py"), "").unwrap();

        // 3. The probe must catch it. This is the assertion the whole change
        //    rests on: no metadata check sees this, because the orphan has no
        //    RECORD and no dist-info, and importlib still reports a healthy
        //    esphome. Only running a real command finds it.
        let detail = esphome_config_probe(&python)
            .expect("probe could not run")
            .expect("the orphaned rp2040 component must fail the health probe");
        assert!(
            detail.contains("rp2040"),
            "probe failed for some other reason: {detail}"
        );

        // 4. Reset the packages.
        let removed = wipe_installed_packages(&python).expect("wipe failed");
        assert!(removed > 0, "the wipe removed nothing");

        // 5. pip must still work. If the wipe takes pip with it the tree is
        //    unrepairable, which is the one truly unrecoverable outcome here.
        let (ok, out) = e2e_run(&python, &["-m", "pip", "--version"]);
        assert!(
            ok,
            "the wipe broke pip, so nothing can be reinstalled: {out}"
        );

        // 6. Everything of ours is gone, orphan included.
        assert!(!orphan.exists(), "the orphan survived the wipe");
        assert!(!purelib.join("esphome").exists());
        let (esphome_gone, _) = e2e_run(&python, &["-m", "esphome", "version"]);
        assert!(!esphome_gone, "esphome survived the wipe");
        let (builder_gone, _) = e2e_run(&python, &["-c", "import esphome_device_builder"]);
        assert!(!builder_gone, "esphome-device-builder survived the wipe");

        // 7. Reinstall, as `reset_python_packages` does after the wipe.
        let (ok, out) = e2e_run(
            &python,
            &["-m", "pip", "install", &format!("esphome=={version}")],
        );
        assert!(ok, "reinstalling esphome=={version} failed: {out}");

        // 8. The tree is healthy again, and the orphan did not come back.
        assert_eq!(
            esphome_config_probe(&python).expect("probe could not run"),
            None,
            "the tree is still broken after the reset"
        );
        assert!(!orphan.exists(), "the orphan came back");
    }

    #[test]
    fn the_system_python_fallback_is_not_a_managed_tree() {
        // `get_python_path` returns a bare command name in dev builds with no
        // bundle. There is no managed tree behind it, so nothing may be swept,
        // and probing it would tell a developer their install is broken when
        // all that is true is that ESPHome is not in their system Python.
        for fallback in ["python3", "python"] {
            assert!(
                python_tree_root(Path::new(fallback)).is_none(),
                "{fallback}"
            );
            assert!(!is_managed_python_tree(Path::new(fallback)), "{fallback}");
        }

        // A real tree still resolves, and is ours to probe.
        let root = unique_temp_dir("tree-root");
        let python = interpreter_in_tree(&root);
        assert_eq!(python_tree_root(&python), Some(root.as_path()));
        assert!(is_managed_python_tree(&python));
        let _ = std::fs::remove_dir_all(&root);
    }
}
