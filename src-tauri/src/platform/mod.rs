//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tracing::debug;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
use ::windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

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
    let user_python = data_dir.join("python");

    // Platform-specific Python path
    #[cfg(target_os = "windows")]
    let python_path = user_python.join("python.exe");

    #[cfg(not(target_os = "windows"))]
    let python_path = user_python.join("bin").join("python3");

    if python_path.exists() {
        debug!("Using user Python: {:?}", python_path);
        return Ok(python_path);
    }

    // Fall back to bundled Python (will be copied on first run)
    let resource_dir = get_bundled_resource_dir(app_handle)?;

    #[cfg(target_os = "windows")]
    let bundled_python = resource_dir.join("python").join("python.exe");

    #[cfg(not(target_os = "windows"))]
    let bundled_python = resource_dir.join("python").join("bin").join("python3");

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

/// Build a `PATH` value with `dir` prepended to `existing`.
///
/// Pure (no environment mutation) so the prepend ordering, separator
/// correctness, and non-Unicode `PATH` preservation can be unit-tested with a
/// synthetic value rather than touching the real process environment — the same
/// split-the-logic pattern `git_check::git_executables_in_path` uses. Going
/// through `split_paths`/`join_paths` keeps the platform separator correct and
/// round-trips a non-Unicode `PATH` instead of lossily dropping it.
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
        use tracing::{info, warn};

        let git_dir = get_bundled_git_dir(app_handle)?;
        let git_exe = git_dir.join("git.exe");
        if !git_exe.exists() {
            warn!(
                "Bundled MinGit missing at {:?}; git-dependent features will \
                 fail until git is on PATH",
                git_exe
            );
            return Ok(());
        }

        // Prepend the bundled git dir to PATH (see path_with_prepended for why
        // it goes through split/join).
        let existing = std::env::var_os("PATH").unwrap_or_default();
        let mut new_path = path_with_prepended(&existing, &git_dir)?;

        // Also expose the bundled GNU patch (issue #189) when present. Only this
        // dedicated dir goes on PATH, not MinGit's full usr/bin, so the build
        // doesn't pick up MSYS sh/find/sort that shadow Windows built-ins.
        let patch_dir = get_bundled_patch_dir(app_handle)?;
        if patch_dir.join("patch.exe").exists() {
            new_path = path_with_prepended(&new_path, &patch_dir)?;
            info!("Using bundled patch at {:?}", patch_dir);
        } else {
            warn!("Bundled patch.exe missing at {:?}; micro-opus and other components that need `patch` will fail to build", patch_dir);
        }

        std::env::set_var("PATH", &new_path);
        info!("Using bundled MinGit at {:?}", git_exe);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = app_handle;
    }

    Ok(())
}

/// Filename of the marker recording which desktop-app version copied the
/// user Python tree. Lives at `<user_python>/.esphome-desktop-version`.
const PYTHON_VERSION_MARKER: &str = ".esphome-desktop-version";

/// Filename of the counter tracking consecutive launches that deferred the
/// bundled-Python refresh because the version probe failed on a still-usable
/// interpreter. Lives inside the user Python tree, so it is reset for free the
/// moment the tree is wiped. See [`MAX_REFRESH_DEFERS`].
const PYTHON_REFRESH_DEFER_MARKER: &str = ".refresh-defer-count";

/// Maximum consecutive refresh defers before forcing the destructive refresh.
/// A usable interpreter whose package metadata is persistently unreadable
/// (e.g. a corrupt `.dist-info`) would otherwise defer on every launch,
/// gating the self-heal behind the very metadata that is broken. After this
/// many defers we stop deferring and wipe to re-copy a clean bundle.
const MAX_REFRESH_DEFERS: u32 = 3;

/// Ensure the user Python exists by copying from bundled Python if needed.
///
/// A version marker file is written into the user Python directory after the
/// copy. On subsequent runs, if the marker is missing or doesn't match the
/// current desktop-app version, the directory is wiped and re-copied so that
/// updated app releases ship a fresh Python tree (e.g. new ESPHome version,
/// changed dependencies). Without this, the first-run copy persisted forever.
pub fn ensure_user_python(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let resource_dir = get_bundled_resource_dir(app_handle)?;
        let bundled_python = resource_dir.join("python").join("python.exe");

        if !bundled_python.exists() {
            anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
        }

        return Ok(());
    }

    #[cfg(not(target_os = "windows"))]
    {
        use tracing::{info, warn};

        let data_dir = get_data_dir(app_handle)?;
        let user_python = data_dir.join("python");
        let python_check = user_python.join("bin").join("python3");
        let marker_path = user_python.join(PYTHON_VERSION_MARKER);
        let current_version = env!("CARGO_PKG_VERSION");

        let marker_matches = std::fs::read_to_string(&marker_path)
            .map(|s| s.trim() == current_version)
            .unwrap_or(false);

        let needs_copy = !python_check.exists() || !marker_matches;

        if needs_copy {
            let resource_dir = get_bundled_resource_dir(app_handle)?;
            let bundled_python = resource_dir.join("python");

            if !bundled_python.exists() {
                anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
            }

            // Snapshot the user's pre-existing package versions BEFORE the
            // wipe so we can restore them after the bundled tree is in place.
            // Without this, a user who pip-bumped ESPHome past the bundled
            // version would silently get downgraded by every app self-update.
            //
            // If the probe FAILS (as opposed to the package being absent), we
            // cannot tell whether the user pinned a newer version, so wiping
            // the tree now would silently discard it — exactly the downgrade
            // this snapshot exists to prevent. In that case defer the refresh:
            // keep the working tree, log a warning, and retry next launch.
            let preserved = if python_check.exists() {
                match snapshot_preserved_versions(&python_check) {
                    Ok(p) => p,
                    Err(e) => {
                        // A probe error means we can't trust a snapshot — but
                        // WHY matters. If the interpreter itself is unusable
                        // (can't even run a trivial script), the tree is broken
                        // and the destructive refresh is the only recovery
                        // path, so fall through and wipe. If the interpreter
                        // runs but the probe failed (non-zero exit, possibly
                        // transient), defer to avoid discarding a user-pinned
                        // version we just couldn't read.
                        //
                        // Deferring is bounded: a usable interpreter whose
                        // package metadata is *persistently* unreadable would
                        // otherwise defer forever, gating the self-heal wipe
                        // behind the very metadata that is broken. After
                        // MAX_REFRESH_DEFERS consecutive defers we proceed with
                        // the wipe to re-copy a clean bundle. The counter lives
                        // inside the tree, so it resets the moment we wipe.
                        if interpreter_is_usable(&python_check) {
                            let defer_marker = user_python.join(PYTHON_REFRESH_DEFER_MARKER);
                            let defers = read_refresh_defer_count(&defer_marker);
                            if defers < MAX_REFRESH_DEFERS
                                && bump_refresh_defer_count(&defer_marker, defers + 1)
                            {
                                warn!(
                                    "Could not read existing Python package versions ({e:#}); \
                                     deferring the bundled-Python refresh to avoid downgrading a \
                                     user-pinned version (defer {}/{}). Will retry on next launch.",
                                    defers + 1,
                                    MAX_REFRESH_DEFERS
                                );
                                return Ok(());
                            }
                            // Either we hit the defer bound, or the counter is
                            // unwritable so it can never advance to that bound.
                            // Both mean "stop deferring and self-heal" — wiping
                            // re-copies a clean bundle and resets the marker.
                            warn!(
                                "Could not read existing Python package versions ({e:#}); the \
                                 package metadata appears persistently broken (or the defer \
                                 counter is unwritable). Wiping and re-copying the bundled tree \
                                 to recover."
                            );
                            PreservedVersions::default()
                        } else {
                            warn!(
                                "Existing Python interpreter at {:?} is unusable ({e:#}); \
                                 wiping and re-copying the bundled tree to recover.",
                                python_check
                            );
                            PreservedVersions::default()
                        }
                    }
                }
            } else {
                PreservedVersions::default()
            };

            if user_python.exists() {
                info!(
                    "Removing stale user Python at {:?} (version marker missing or mismatched)",
                    user_python
                );
                std::fs::remove_dir_all(&user_python)
                    .context("Failed to remove stale user Python directory")?;
            }

            info!(
                "Copying bundled Python to user data directory (version {})...",
                current_version
            );

            // Copy the bundled Python to user data
            copy_dir_recursive(&bundled_python, &user_python)?;

            // Atomic write: a torn marker could read back as a partial version
            // string, mismatching on next launch and re-copying the whole tree.
            crate::util::atomic_write(&marker_path, current_version)
                .context("Failed to write Python version marker")?;

            restore_preserved_versions(&python_check, &preserved);

            info!("User Python ready at {:?}", user_python);
        } else {
            debug!(
                "User Python already up-to-date (version {})",
                current_version
            );
        }

        Ok(())
    }
}

/// User-preferred package versions captured before the bundled Python tree
/// is wiped during an app-version refresh. See [`ensure_user_python`].
#[derive(Debug, Default)]
struct PreservedVersions {
    esphome: Option<String>,
    esphome_device_builder: Option<String>,
}

/// Snapshot the user-pinned versions of the packages we preserve across a
/// bundled-Python refresh. Returns `Err` if any probe FAILS (a `None` from
/// [`read_package_version`] means the package is genuinely absent, which is a
/// successful snapshot). The caller must not wipe a tree it could not read, or
/// it would silently downgrade a version the user deliberately pinned.
#[cfg(not(target_os = "windows"))]
fn snapshot_preserved_versions(python_bin: &Path) -> Result<PreservedVersions> {
    Ok(PreservedVersions {
        esphome: read_package_version(python_bin, "esphome")?,
        esphome_device_builder: read_package_version(python_bin, "esphome-device-builder")?,
    })
}

/// Returns `true` if the interpreter can run a trivial script with a clean
/// exit. A `false` result means the tree is broken badly enough (interpreter
/// can't spawn or can't execute at all) that the destructive bundled-Python
/// refresh is the right recovery, rather than deferring forever and leaving a
/// corrupt tree with no automatic repair path. Used to split a transient probe
/// error (defer) from a genuinely unusable interpreter (wipe & recover).
#[cfg(not(target_os = "windows"))]
fn interpreter_is_usable(python_bin: &Path) -> bool {
    let mut cmd = std::process::Command::new(python_bin);
    cmd.args(["-c", "pass"]);
    configure_no_window_command(&mut cmd);
    matches!(cmd.output(), Ok(o) if o.status.success())
}

/// Read the consecutive-defer counter, returning 0 when the marker is missing
/// or unparseable (treat a damaged counter as a fresh start rather than
/// blocking the self-heal wipe).
#[cfg(not(target_os = "windows"))]
fn read_refresh_defer_count(marker_path: &Path) -> u32 {
    std::fs::read_to_string(marker_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Persist the consecutive-defer counter. Returns `true` if the new count was
/// durably written. A `false` (write failed) means the counter can't advance,
/// so the caller must NOT defer again — otherwise a persistently unwritable
/// marker would re-introduce the defer-forever shape this counter exists to
/// bound, just triggered by a failed write instead of a failed read.
#[cfg(not(target_os = "windows"))]
fn bump_refresh_defer_count(marker_path: &Path, count: u32) -> bool {
    match crate::util::atomic_write(marker_path, count.to_string()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Could not persist refresh-defer counter to {marker_path:?}: {e:#}");
            false
        }
    }
}

/// Reinstall any preserved package whose pinned version is newer than the
/// version that just shipped in the new bundled Python tree. Bundled wins
/// for ties and for when bundled is newer (so users always benefit from the
/// app's fresher bundle when they haven't explicitly bumped past it). Each
/// reinstall is best-effort — a network failure here logs a warning and
/// falls through to the bundled version rather than blocking app start.
#[cfg(not(target_os = "windows"))]
fn restore_preserved_versions(python_bin: &Path, preserved: &PreservedVersions) {
    use tracing::{info, warn};

    for (package, saved) in [
        ("esphome", preserved.esphome.as_deref()),
        (
            "esphome-device-builder",
            preserved.esphome_device_builder.as_deref(),
        ),
    ] {
        let Some(saved) = saved else { continue };
        let bundled = match read_package_version(python_bin, package) {
            Ok(Some(v)) => v,
            Ok(None) => {
                // Package isn't in the bundled tree (shouldn't happen for these
                // two, but don't fight it). Skip the restore.
                continue;
            }
            Err(e) => {
                // Couldn't read the freshly-copied bundled version, so we can't
                // compare. Skip rather than blindly reinstall (which might
                // downgrade if bundled is actually newer).
                warn!(
                    "Could not read bundled {package} version ({e:#}); skipping {saved} restore."
                );
                continue;
            }
        };
        if !crate::update::is_newer_version(saved, &bundled) {
            debug!(
                "Bundled {} {} satisfies user preference {}; not reinstalling",
                package, bundled, saved
            );
            continue;
        }
        info!(
            "Restoring user-preferred {} {} over bundled {}",
            package, saved, bundled
        );
        if let Err(e) = pip_install_blocking(python_bin, package, saved) {
            warn!(
                "Failed to restore {} {}: {}. Continuing with bundled {}.",
                package, saved, e, bundled
            );
        }
    }
}

/// Read the installed version of a Python package via `importlib.metadata`.
///
/// Returns:
/// - `Ok(Some(v))` — installed at version `v`.
/// - `Ok(None)` — confirmed not installed (`PackageNotFoundError`).
/// - `Err(_)` — the probe itself failed (couldn't spawn the interpreter, or it
///   exited non-zero on an unexpected exception). This is deliberately distinct
///   from "not installed": callers that snapshot versions before a destructive
///   refresh must not treat a flaky probe as "absent" — see
///   [`snapshot_preserved_versions`].
#[cfg(not(target_os = "windows"))]
fn read_package_version(python_bin: &Path, package: &str) -> Result<Option<String>> {
    // Written as a single-line literal with explicit `\n` so each Python
    // statement starts at column zero — avoids any ambiguity about whether
    // a Rust line-continuation strips the source-line indentation. A clean
    // exit with no output means PackageNotFoundError; any other exception
    // propagates as a non-zero exit and is surfaced as an error below.
    let script = format!(
        "from importlib.metadata import version, PackageNotFoundError\ntry: print(version('{}'))\nexcept PackageNotFoundError: pass",
        package
    );
    let mut cmd = std::process::Command::new(python_bin);
    cmd.args(["-c", &script]);
    configure_no_window_command(&mut cmd);
    let output = cmd
        .output()
        .with_context(|| format!("Failed to run version probe for {package} via {python_bin:?}"))?;
    parse_probe_output(
        package,
        output.status.success(),
        &output.stdout,
        &output.stderr,
    )
    .with_context(|| format!("version probe for {package} via {python_bin:?}"))
}

/// Pure parser for [`read_package_version`]'s subprocess result. A successful
/// run with empty stdout means the package is absent (`Ok(None)`); a non-empty
/// stdout yields the trimmed version; a failed run is an error carrying the
/// (tail-truncated) stderr.
#[cfg(not(target_os = "windows"))]
fn parse_probe_output(
    package: &str,
    success: bool,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Option<String>> {
    if !success {
        let stderr = String::from_utf8_lossy(stderr);
        anyhow::bail!(
            "version probe for {package} exited non-zero: {}",
            tail_for_log(&stderr)
        );
    }
    let v = String::from_utf8_lossy(stdout).trim().to_string();
    Ok(if v.is_empty() { None } else { Some(v) })
}

/// Hard upper bound on a single `pip install` invocation during the
/// version-restore path. Five minutes is well over the time needed to
/// upgrade `esphome` on a working connection; bounding it prevents a
/// stalled network from hanging app startup indefinitely.
#[cfg(not(target_os = "windows"))]
const PIP_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Maximum length of pip stderr included in a failure error message. pip's
/// resolver and progress output can run to many kilobytes; the actionable
/// failure reason is almost always at the tail, so we truncate to the last
/// N bytes to keep log lines (and downstream UI surfaces) bounded.
#[cfg(not(target_os = "windows"))]
const PIP_STDERR_TAIL_BYTES: usize = 4096;

/// Return `s` trimmed and truncated to the last [`PIP_STDERR_TAIL_BYTES`]
/// bytes, with a marker line if anything was dropped. Backs up to a UTF-8
/// char boundary so the result is always valid `str`.
#[cfg(not(target_os = "windows"))]
fn tail_for_log(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= PIP_STDERR_TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - PIP_STDERR_TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "...(stderr truncated to last {} bytes)\n{}",
        PIP_STDERR_TAIL_BYTES,
        &trimmed[start..]
    )
}

/// Synchronously run `pip install <package>==<version>` with a wall-clock
/// timeout. Pinning the exact version lets pip resolve pre-releases without
/// needing `--pre`. On timeout the child is killed and an error is returned;
/// the caller logs a warning and falls back to the bundled version, so a
/// stalled pip can't block app launch.
#[cfg(not(target_os = "windows"))]
fn pip_install_blocking(python_bin: &Path, package: &str, version: &str) -> Result<()> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let spec = format!("{}=={}", package, version);
    let mut cmd = std::process::Command::new(python_bin);
    cmd.args(["-m", "pip", "install", &spec]);
    cmd.stderr(std::process::Stdio::piped());
    configure_no_window_command(&mut cmd);

    let mut child = cmd.spawn().context("Failed to spawn pip install")?;
    let deadline = Instant::now() + PIP_INSTALL_TIMEOUT;

    // Drain stderr in a background thread. pip's progress bars and resolver
    // diagnostics can easily exceed the OS pipe buffer (~64 KiB on Linux);
    // if nothing reads the parent's end, pip blocks on `write()` mid-install,
    // which would defeat the deadline and hang startup. The reader exits
    // naturally once the child closes its stderr fd (normal exit or kill).
    let mut stderr_thread = child.stderr.take().map(|mut handle| {
        std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = handle.read_to_string(&mut buf);
            buf
        })
    });

    loop {
        match child.try_wait().context("Failed to poll pip install")? {
            Some(status) => {
                let stderr = stderr_thread
                    .take()
                    .and_then(|t| t.join().ok())
                    .unwrap_or_default();
                if status.success() {
                    return Ok(());
                }
                anyhow::bail!("pip install {} failed: {}", spec, tail_for_log(&stderr));
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let stderr = stderr_thread
                        .take()
                        .and_then(|t| t.join().ok())
                        .unwrap_or_default();
                    anyhow::bail!(
                        "pip install {} timed out after {:?}; partial stderr: {}",
                        spec,
                        PIP_INSTALL_TIMEOUT,
                        tail_for_log(&stderr)
                    );
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
}

/// Recursively copy a directory, preserving symlinks.
///
/// Uses [`std::fs::DirEntry::file_type`] — which does NOT follow symlinks — so
/// that links in the source tree are recreated as links in the destination
/// rather than dereferenced. This matters for the bundled Python tree, which on
/// macOS/Linux relies on symlinks (framework `Current` links, versioned
/// `libpython*.so`/`*.dylib`, etc.). The previous implementation used
/// `Path::is_dir()`/`fs::copy`, both of which follow symlinks: that bloated the
/// copy, flattened the framework layout, and — for a *dangling* link — made
/// `fs::copy` fail with "No such file", aborting the entire copy and leaving the
/// app unable to start.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    use std::fs;

    if !dst.exists() {
        fs::create_dir_all(dst).context("Failed to create destination directory")?;
    }

    for entry in fs::read_dir(src).context("Failed to read source directory")? {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        let file_type = entry.file_type().context("Failed to read file type")?;

        if file_type.is_symlink() {
            copy_symlink(&path, &dest_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            fs::copy(&path, &dest_path).context("Failed to copy file")?;
        }
    }

    Ok(())
}

/// Recreate the symlink at `src` under `dst`, pointing at the same (possibly
/// relative, possibly dangling) target. The stored target string is copied
/// verbatim — never resolved or followed — so link semantics survive the copy.
/// On Windows the source-side target is inspected only to pick the link *type*
/// (`symlink_dir` vs `symlink_file`); the stored target itself is left unchanged.
fn copy_symlink(src: &Path, dst: &Path) -> Result<()> {
    let target = std::fs::read_link(src).context("Failed to read symlink target")?;

    // Make re-copies idempotent: drop any pre-existing entry at the destination.
    // A real directory needs `remove_dir_all`; a *directory symlink* needs
    // `remove_dir` (on Windows `remove_file` cannot delete it); everything else
    // (file, file symlink) uses `remove_file`. Leaving a stale entry in place
    // would make the later symlink call fail with `AlreadyExists`.
    if let Ok(meta) = dst.symlink_metadata() {
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            // A directory symlink must be removed with `remove_dir` on Windows;
            // `remove_file` works for file symlinks on all platforms. Try
            // `remove_file` first, then fall back to `remove_dir`.
            if std::fs::remove_file(dst).is_err() {
                let _ = std::fs::remove_dir(dst);
            }
        } else if file_type.is_dir() {
            let _ = std::fs::remove_dir_all(dst);
        } else {
            let _ = std::fs::remove_file(dst);
        }
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst).context("Failed to create symlink")?;
    }

    #[cfg(windows)]
    {
        // Windows requires the link type to match the target. Probe the *source*
        // side, where the full tree exists and the target is guaranteed
        // resolvable — probing the partially-populated destination could pick the
        // wrong link type if the target dir hasn't been copied yet.
        let probe = if target.is_absolute() {
            target.clone()
        } else {
            src.parent()
                .map(|p| p.join(&target))
                .unwrap_or_else(|| target.clone())
        };
        if probe.is_dir() {
            std::os::windows::fs::symlink_dir(&target, dst)
                .context("Failed to create directory symlink")?;
        } else {
            std::os::windows::fs::symlink_file(&target, dst)
                .context("Failed to create file symlink")?;
        }
    }

    Ok(())
}

/// Check if ESPHome is available (bundled Python has it pre-installed)
pub fn is_esphome_ready(app_handle: &AppHandle) -> bool {
    let python_path = match get_python_path(app_handle) {
        Ok(p) => p,
        Err(_) => return false,
    };

    // Try to run esphome version
    let mut cmd = std::process::Command::new(&python_path);
    cmd.args(["-m", "esphome", "version"]);
    configure_no_window_command(&mut cmd);

    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

/// Configure std::process::Command to not create a console window on Windows
pub fn configure_no_window_command(cmd: &mut std::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Configure tokio::process::Command to not create a console window on Windows
pub fn configure_no_window_tokio_command(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Configure the daemon child's creation flags on Windows: no console window
/// AND a new process group. The new process group makes the child its own
/// group leader (pgid == pid) so we can later deliver a graceful
/// `CTRL_BREAK_EVENT` to it (and its descendants) for shutdown via
/// `send_ctrl_break`. Sets both flags in one call so neither overwrites the
/// other. No-op on non-Windows (Unix uses `process_group(0)` instead).
pub fn configure_daemon_tokio_command(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags((CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Deliver a graceful `CTRL_BREAK_EVENT` to a child process group on Windows.
///
/// Returns `true` if the event was delivered, `false` if it could not be (the
/// child already exited, or its console is unreachable) — the caller should
/// then fall back to `TerminateProcess`.
///
/// `pid` must be the PID of a child spawned with `CREATE_NEW_PROCESS_GROUP`
/// (see `configure_daemon_tokio_command`); for such a child the process-group
/// id equals its PID. `CTRL_BREAK_EVENT` is the only usable signal here:
/// `CREATE_NEW_PROCESS_GROUP` disables CTRL+C for the group, and unlike
/// `CTRL_C_EVENT` a break can target a specific group id.
///
/// The desktop app is a GUI process with no console, so a bare
/// `GenerateConsoleCtrlEvent` would have nothing to signal through. We
/// transiently attach to the child's (hidden) console, suppress the event in
/// ourselves so we don't self-terminate, broadcast it, then detach. This
/// mutates whole-process console state, so it is serialized under a lock; it
/// is also known to be finicky, hence the caller's `TerminateProcess`
/// fallback.
///
/// A release build is a GUI (windows-subsystem) process and owns no console,
/// so the detach is a no-op. A dev/console build run from a terminal (so the
/// daemon's tracing is visible) does own one; detaching it would tear that
/// terminal down, so we record it up front and reattach to it before
/// returning on every exit path.
#[cfg(target_os = "windows")]
pub fn send_ctrl_break(pid: u32) -> bool {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleWindow, GetStdHandle,
        SetConsoleCtrlHandler, SetStdHandle, ATTACH_PARENT_PROCESS, CTRL_BREAK_EVENT,
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    // Serialize: AttachConsole/FreeConsole/SetConsoleCtrlHandler mutate
    // per-process (not per-thread) console state, so two concurrent sends
    // would corrupt each other.
    static CONSOLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY: serialized Win32 console FFI. We restore the ctrl handler, the
    // standard handles, and our original console attachment before returning
    // regardless of outcome; no handle or console state escapes this function.
    unsafe {
        // Record whether we own a real console (one with a window) before we
        // touch any console state. A GUI release build owns none, so this is
        // false and the detach below is a no-op. A dev/console build run from
        // a terminal owns one; we reattach to it on the way out so a shutdown
        // attempt doesn't tear the terminal down.
        let had_console = !GetConsoleWindow().0.is_null();

        // Save our standard handles up front and restore them on every exit
        // path. AttachConsole/FreeConsole mutate whole-process console state
        // and leave this (GUI, console-less) process's STD_INPUT_HANDLE
        // dangling — NULL at launch, but an invalid non-NULL value once we
        // attach to and then free the child's console. Anything we spawn after
        // a shutdown attempt (notably the daemon respawn on restart) would then
        // inherit that invalid handle, and because the daemon command
        // redirects stdout/stderr (setting STARTF_USESTDHANDLES, which requires
        // all three standard handles to be valid) CreateProcess fails with
        // ERROR_INVALID_HANDLE. Restoring the saved values keeps our handle
        // state exactly as it was before the call. (The daemon command also
        // pins stdin to NUL as a belt-and-suspenders measure; this restore
        // protects any other post-shutdown spawn too.)
        //
        // GetStdHandle returns Err only for INVALID_HANDLE_VALUE; a console-
        // less process legitimately has NULL standard handles, which come back
        // as Ok(NULL). We coerce either case to a concrete HANDLE and restore
        // it unconditionally, so a process that started with NULL handles ends
        // with NULL handles rather than whatever the console churn left behind.
        let null_handle = HANDLE(std::ptr::null_mut());
        let saved_in = GetStdHandle(STD_INPUT_HANDLE).unwrap_or(null_handle);
        let saved_out = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or(null_handle);
        let saved_err = GetStdHandle(STD_ERROR_HANDLE).unwrap_or(null_handle);
        let restore = || {
            // Reattach to our original (parent's) console first for dev/console
            // builds; AttachConsole resets the standard handles, so the handle
            // restore must come after it.
            if had_console {
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            }
            let _ = SetStdHandle(STD_INPUT_HANDLE, saved_in);
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, saved_out);
            let _ = SetStdHandle(STD_ERROR_HANDLE, saved_err);
        };

        // Detach from any console we currently hold; otherwise AttachConsole
        // fails with ERROR_ACCESS_DENIED (a process can attach to at most one
        // console). Harmless if we have none.
        let _ = FreeConsole();
        if AttachConsole(pid).is_err() {
            // Child gone, or its console is not reachable.
            restore();
            return false;
        }
        // Make ourselves ignore the event we are about to broadcast so we
        // don't terminate the desktop along with the child. AttachConsole
        // resets the handler table, so this must come after it.
        let _ = SetConsoleCtrlHandler(None, true);
        let delivered = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid).is_ok();
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = FreeConsole();
        restore();
        delivered
    }
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

#[cfg(target_os = "macos")]
mod macos {
    use tauri::{ActivationPolicy, AppHandle};

    pub fn init(app_handle: &AppHandle) {
        // Tray-only app with no windows: mark it as an accessory app so it
        // doesn't appear in the Dock or the Cmd+Tab switcher.
        if let Err(e) = app_handle.set_activation_policy(ActivationPolicy::Accessory) {
            tracing::warn!("Failed to set macOS activation policy to Accessory: {e}");
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    pub fn init() {
        // Windows-specific initialization
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::path::{Path, PathBuf};
    use tracing::{debug, info, warn};

    pub fn init() {
        // Linux-specific initialization
    }

    /// Shared-library sonames that provide a usable appindicator backend,
    /// most-preferred first. ayatana is the maintained fork; the legacy
    /// `libappindicator3` names are kept for older distributions.
    const APPINDICATOR_SONAMES: &[&str] = &[
        "libayatana-appindicator3.so.1",
        "libappindicator3.so.1",
        "libayatana-appindicator3.so",
        "libappindicator3.so",
    ];

    /// Directories, relative to `APPDIR`, where a sharun-based AppImage may
    /// place bundled shared libraries. `shared/lib` is sharun's default; the
    /// rest cover multiarch / `usr`-prefixed layouts seen in the wild.
    const APPDIR_LIB_DIRS: &[&str] = &[
        "shared/lib",
        "shared/lib/x86_64-linux-gnu",
        "shared/lib/aarch64-linux-gnu",
        "usr/lib",
        "usr/lib/x86_64-linux-gnu",
        "usr/lib/aarch64-linux-gnu",
        "usr/lib64",
        "lib",
    ];

    /// Build the list of absolute paths where a bundled appindicator library
    /// might live inside an AppImage rooted at `appdir`.
    fn appindicator_candidate_paths(appdir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::with_capacity(APPDIR_LIB_DIRS.len() * APPINDICATOR_SONAMES.len());
        for dir in APPDIR_LIB_DIRS {
            for name in APPINDICATOR_SONAMES {
                paths.push(appdir.join(dir).join(name));
            }
        }
        paths
    }

    /// Locate a bundled appindicator library inside an AppImage's `APPDIR`,
    /// returning the first existing candidate path.
    fn find_bundled_appindicator(appdir: &Path) -> Option<PathBuf> {
        appindicator_candidate_paths(appdir)
            .into_iter()
            .find(|p| p.exists())
    }

    /// Check if a usable appindicator library is available on this system.
    ///
    /// The `tray-icon` crate (via `libappindicator-sys`) lazily `dlopen`s the
    /// library by bare soname and will `panic!()` if it cannot be loaded. We
    /// probe for it first so we can degrade gracefully instead of crashing.
    ///
    /// On a sharun-based AppImage the bundled library is not on the loader's
    /// default search path, and sharun sets `DT_RUNPATH` (which `dlopen`
    /// ignores when resolving a bare soname), so the plain soname probe gets a
    /// false negative — suppressing the tray even on desktops that fully
    /// support it (e.g. KDE Plasma, issue #87). To handle that we locate the
    /// bundled copy by absolute path and load it, leaving it resident so the
    /// crate's later bare-soname `dlopen` resolves to the already-loaded object.
    pub fn is_appindicator_available() -> bool {
        use std::ffi::OsStr;

        // 1. Standard probe: resolve by bare soname through the loader's
        //    default search path. Succeeds on deb/rpm/AUR installs (and any
        //    system with the library installed normally).
        for name in APPINDICATOR_SONAMES {
            if unsafe { libloading::Library::new(OsStr::new(name)) }.is_ok() {
                return true;
            }
        }

        // 2. AppImage fallback: find the bundled library by absolute path and
        //    load it. The dynamic linker matches an already-loaded object by
        //    its `DT_SONAME`, so priming it here makes `libappindicator-sys`'s
        //    later bare-soname `dlopen` succeed instead of panicking. We
        //    deliberately leak the handle (`mem::forget`) so the library stays
        //    resident for the lifetime of the process.
        if let Ok(appdir) = std::env::var("APPDIR") {
            match find_bundled_appindicator(Path::new(&appdir)) {
                Some(lib_path) => match unsafe { libloading::Library::new(&lib_path) } {
                    Ok(lib) => {
                        info!("Loaded bundled appindicator from {:?}", lib_path);
                        std::mem::forget(lib);
                        return true;
                    }
                    Err(e) => {
                        warn!(
                            "Found bundled appindicator at {:?} but it failed to load: {}",
                            lib_path, e
                        );
                    }
                },
                None => debug!("APPDIR set but no bundled appindicator library found"),
            }
        }

        false
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::sync::atomic::{AtomicU64, Ordering};

        /// Unique temp dir per call so parallel tests never collide.
        fn unique_temp_dir(tag: &str) -> PathBuf {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            std::env::temp_dir().join(format!(
                "koan-appindicator-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ))
        }

        #[test]
        fn candidate_paths_are_all_rooted_in_appdir() {
            let appdir = Path::new("/tmp/.mount_abc");
            let paths = appindicator_candidate_paths(appdir);
            assert!(!paths.is_empty());
            assert!(paths.iter().all(|p| p.starts_with(appdir)));
            // Every candidate must end with one of the known sonames.
            assert!(paths.iter().all(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                APPINDICATOR_SONAMES.contains(&name)
            }));
        }

        #[test]
        fn candidate_paths_include_sharun_default_layout() {
            let appdir = Path::new("/opt/app");
            let paths = appindicator_candidate_paths(appdir);
            // sharun's default `shared/lib` plus the preferred ayatana soname.
            assert!(paths.contains(&appdir.join("shared/lib/libayatana-appindicator3.so.1")));
        }

        #[test]
        fn find_bundled_appindicator_locates_existing_library() {
            let appdir = unique_temp_dir("found");
            let lib_dir = appdir.join("shared/lib");
            fs::create_dir_all(&lib_dir).unwrap();
            let lib = lib_dir.join("libayatana-appindicator3.so.1");
            fs::write(&lib, b"\x7fELF").unwrap();

            assert_eq!(find_bundled_appindicator(&appdir), Some(lib));

            let _ = fs::remove_dir_all(&appdir);
        }

        #[test]
        fn find_bundled_appindicator_returns_none_when_absent() {
            let appdir = unique_temp_dir("absent");
            fs::create_dir_all(&appdir).unwrap();

            assert_eq!(find_bundled_appindicator(&appdir), None);

            let _ = fs::remove_dir_all(&appdir);
        }
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
                .message(
                    "An older version of this app named \u{201C}ESPHome Builder\u{201D} \
                     was found in your Applications folder. Move it to the Trash?\n\n\
                     Your settings and configs are not affected.",
                )
                .title("Remove old ESPHome Builder")
                .kind(MessageDialogKind::Info)
                .buttons(MessageDialogButtons::OkCancelCustom(
                    "Move to Trash".to_string(),
                    "Keep".to_string(),
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
    use super::*;

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

    /// Unique temp dir per call. Combines the process id with a monotonic
    /// counter so tests running in parallel within the same process can never
    /// collide on (or delete) each other's directories.
    #[cfg(unix)]
    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "koan-copytest-{}-{}-{}",
            tag,
            std::process::id(),
            n
        ))
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_preserves_symlinks() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("basic");
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();

        fs::write(src.join("real.txt"), b"hello").unwrap();
        symlink("real.txt", src.join("link.txt")).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        let copied = dst.join("link.txt");
        let meta = fs::symlink_metadata(&copied).unwrap();
        assert!(
            meta.file_type().is_symlink(),
            "symlink must be preserved, not dereferenced into a regular file"
        );
        assert_eq!(fs::read_link(&copied).unwrap(), Path::new("real.txt"));
        assert_eq!(fs::read_to_string(&copied).unwrap(), "hello");

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_tolerates_dangling_symlink() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("dangling");
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();

        // A link to a nonexistent target. The old dereferencing copy would
        // abort the whole operation here with "No such file".
        symlink("does-not-exist", src.join("dangling")).unwrap();
        fs::write(src.join("after.txt"), b"copied anyway").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(fs::symlink_metadata(dst.join("dangling"))
            .unwrap()
            .file_type()
            .is_symlink());
        // A sibling visited after the dangling link must still be copied.
        assert_eq!(
            fs::read_to_string(dst.join("after.txt")).unwrap(),
            "copied anyway"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_preserves_nested_symlinked_dir_target() {
        use std::fs;
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("nested");
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(src.join("versions/3.13")).unwrap();
        fs::write(src.join("versions/3.13/file"), b"v").unwrap();
        // Framework-style "Current -> 3.13" directory symlink.
        symlink("3.13", src.join("versions/Current")).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        let current = dst.join("versions/Current");
        assert!(
            fs::symlink_metadata(&current)
                .unwrap()
                .file_type()
                .is_symlink(),
            "directory symlink must stay a symlink, not be recursed into and duplicated"
        );
        assert_eq!(fs::read_link(&current).unwrap(), Path::new("3.13"));

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn tail_for_log_passes_short_input_through_trimmed() {
        assert_eq!(tail_for_log("  hello  "), "hello");
        assert_eq!(tail_for_log("plain"), "plain");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn parse_probe_output_reports_version() {
        let v = parse_probe_output("esphome", true, b"2026.5.0\n", b"").unwrap();
        assert_eq!(v, Some("2026.5.0".to_string()));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn tail_for_log_keeps_input_at_exactly_the_limit() {
        let s = "a".repeat(PIP_STDERR_TAIL_BYTES);
        let out = tail_for_log(&s);
        assert_eq!(out, s, "input exactly at the limit must pass through");
        assert!(!out.contains("truncated"), "no marker at the boundary");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn parse_probe_output_empty_means_absent() {
        let v = parse_probe_output("esphome", true, b"", b"").unwrap();
        assert_eq!(v, None, "clean exit with no output means not installed");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn tail_for_log_truncates_to_the_tail_with_marker() {
        let s = "x".repeat(PIP_STDERR_TAIL_BYTES + 904);
        let out = tail_for_log(&s);
        assert!(
            out.starts_with("...(stderr truncated"),
            "marker comes first"
        );
        assert!(
            out.ends_with(&s[s.len() - PIP_STDERR_TAIL_BYTES..]),
            "keeps tail"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn tail_for_log_does_not_split_a_multibyte_char() {
        // 1366 * 3 bytes = 4098 > 4096; the naive cut at len-4096 lands at
        // byte 2, mid-"€". The function advances past the partial leading
        // char to the next char boundary, so the result stays valid UTF-8
        // and never panics.
        let s = "€".repeat(1366);
        let out = tail_for_log(&s);
        assert!(out.contains("truncated"), "long input must be marked");
        let tail = out.split_once('\n').unwrap().1;
        assert!(
            tail.len() <= PIP_STDERR_TAIL_BYTES,
            "tail stays within bound"
        );
        assert!(tail.chars().all(|c| c == '€'), "no partial char survives");
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn parse_probe_output_failure_is_error_not_absent() {
        // A non-zero exit must NOT be conflated with "not installed" — that
        // conflation would let a flaky probe silently discard a user-pinned
        // version during the bundled-Python refresh.
        let err = parse_probe_output("esphome", false, b"", b"Traceback: boom").unwrap_err();
        assert!(
            err.to_string().contains("esphome"),
            "error names the package"
        );
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn refresh_defer_count_missing_marker_is_zero() {
        let base = unique_temp_dir("defer-missing");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            read_refresh_defer_count(&base.join(".refresh-defer-count")),
            0
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn refresh_defer_count_round_trips_and_bounds_defers() {
        // A persistently failing probe must stop deferring after the bound,
        // so the destructive self-heal wipe can run instead of looping forever.
        let base = unique_temp_dir("defer-bound");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let marker = base.join(".refresh-defer-count");

        let mut count = read_refresh_defer_count(&marker);
        let mut defers = 0;
        while count < MAX_REFRESH_DEFERS {
            bump_refresh_defer_count(&marker, count + 1);
            count = read_refresh_defer_count(&marker);
            defers += 1;
        }
        assert_eq!(defers, MAX_REFRESH_DEFERS, "defers are bounded");
        assert_eq!(count, MAX_REFRESH_DEFERS, "counter persists across reads");

        let _ = std::fs::remove_dir_all(&base);
    }
}
