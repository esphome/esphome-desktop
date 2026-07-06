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
#[derive(Clone, Copy)]
enum PathInsert {
    /// Prepend, so the dir shadows anything already on `PATH`. For bundled tools
    /// we always want to win (MinGit, the bundled ccache).
    Front,
    /// Append, so the dir is only a fallback and never shadows an earlier entry.
    /// For the Homebrew dirs on macOS.
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
pub fn ensure_user_python(app_handle: &AppHandle, force_device_builder: bool) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let _ = force_device_builder;
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
            // When `force_device_builder` is set (a user migrating off the
            // removed classic dashboard), `esphome-device-builder` is left out
            // of the snapshot so the freshly bundled copy always wins and the
            // user lands on the current device builder.
            //
            // If the probe FAILS (as opposed to the package being absent), we
            // cannot tell whether the user pinned a newer version, so wiping
            // the tree now would silently discard it — exactly the downgrade
            // this snapshot exists to prevent. In that case defer the refresh:
            // keep the working tree, log a warning, and retry next launch.
            let preserved = if python_check.exists() {
                match snapshot_preserved_versions(&python_check, force_device_builder) {
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
                        //
                        // Never defer while forcing the device builder (classic
                        // migration): the daemon now always launches
                        // `esphome_device_builder`, and an old classic tree may
                        // not have it, so we must refresh to the bundle that
                        // does rather than keep the old tree another launch.
                        if !force_device_builder && interpreter_is_usable(&python_check) {
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
                        } else if force_device_builder {
                            warn!(
                                "Could not read existing Python package versions ({e:#}) while \
                                 migrating off the classic backend; refreshing to the bundled tree."
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
///
/// With `force_device_builder`, `esphome-device-builder` is excluded from the
/// snapshot so the freshly bundled copy is kept as-is on restore (and a probe
/// failure for it can't trigger a refresh defer either). Used to move a user
/// off the removed classic dashboard onto the current device builder.
#[cfg(not(target_os = "windows"))]
fn snapshot_preserved_versions(
    python_bin: &Path,
    force_device_builder: bool,
) -> Result<PreservedVersions> {
    Ok(PreservedVersions {
        esphome: read_package_version(python_bin, "esphome")?,
        esphome_device_builder: if force_device_builder {
            None
        } else {
            read_package_version(python_bin, "esphome-device-builder")?
        },
    })
}

/// Returns `true` if the interpreter can import the metadata machinery the
/// version probe depends on ([`read_package_version`]'s script starts with
/// `importlib.metadata`, whose import chain pulls in `re`, `enum`, `types`,
/// ...). A `false` result means the tree is broken badly enough (interpreter
/// can't spawn, or its stdlib is corrupt so no probe can ever succeed) that
/// the destructive bundled-Python refresh is the right recovery, rather than
/// deferring forever and leaving a corrupt tree with no automatic repair path.
/// Used to split a transient probe error (defer) from a genuinely unusable
/// interpreter (wipe & recover). A bare `-c "pass"` is NOT enough here: a
/// gutted stdlib still executes it cleanly while every import fails.
#[cfg(not(target_os = "windows"))]
fn interpreter_is_usable(python_bin: &Path) -> bool {
    matches!(
        run_python_capture(python_bin, ["-c", "import importlib.metadata"]),
        Ok(o) if o.status.success()
    )
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
    let output = run_python_capture(python_bin, ["-c", &script])
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
    run_python_capture(&python_path, ["-m", "esphome", "version"])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Spawn the given Python interpreter with `args`, suppress the console
/// window on Windows, and capture its output. This only removes the
/// spawn/capture boilerplate: it adds no flags of its own (callers pass
/// exactly the flags they need, `-I` included or not), and callers keep their
/// own policy for exit status, logging, and stdout/stderr interpretation.
pub fn run_python_capture<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::io::Result<std::process::Output> {
    let mut cmd = std::process::Command::new(python);
    cmd.args(args);
    configure_no_window_command(&mut cmd);
    cmd.output()
}

/// [`run_python_capture`], returning the trimmed stdout on a successful exit
/// and `None` on a non-zero exit. stderr is captured but not returned, so
/// callers that need it (or the exit status) should use
/// [`run_python_capture`] directly.
pub fn run_python_capture_stdout<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::io::Result<Option<String>> {
    let output = run_python_capture(python, args)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
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

/// Build a tokio `pip install` command for the given Python interpreter,
/// prefilled with `-m pip install` and the Windows no-window flag. Callers
/// append their own package specs and flags before running it.
pub fn pip_command(python: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(python);
    cmd.args(["-m", "pip", "install"]);
    configure_no_window_tokio_command(&mut cmd);
    cmd
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

#[cfg(target_os = "macos")]
mod macos {
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use tauri::{ActivationPolicy, AppHandle};

    pub fn init(app_handle: &AppHandle) {
        // Tray-only app with no windows: mark it as an accessory app so it
        // doesn't appear in the Dock or the Cmd+Tab switcher.
        if let Err(e) = app_handle.set_activation_policy(ActivationPolicy::Accessory) {
            tracing::warn!("Failed to set macOS activation policy to Accessory: {e}");
        }

        install_cli_command();
    }

    /// Candidate PATH directories for the `esphome-desktop` shell command,
    /// most-preferred first. Both are on the default PATH (`/usr/local/bin`
    /// via `/etc/paths`, `/opt/homebrew/bin` via Homebrew's shellenv) and are
    /// typically admin-user writable on machines with Homebrew installed.
    const CLI_COMMAND_DIRS: &[&str] = &["/opt/homebrew/bin", "/usr/local/bin"];

    /// Name of the shell command; must match the binary name the CLI parses as.
    const CLI_NAME: &str = "esphome-desktop";

    /// Marker identifying a wrapper as ours, so refreshes never touch a
    /// user's own script or binary that happens to share the name.
    const CLI_MARKER: &str = "auto-generated by ESPHome Device Builder";

    /// What [`try_install_command_in`] found in one candidate directory.
    enum CommandOutcome {
        /// Wrote or refreshed the wrapper.
        Installed,
        /// A current wrapper for this binary is already in place.
        AlreadyInstalled,
        /// Something that isn't ours occupies the name; leave it alone.
        Foreign,
        /// Directory missing or not writable; try the next candidate.
        Unavailable,
    }

    /// Install (or refresh) the `esphome-desktop` shell command so the CLI
    /// control channel works from a terminal without hunting down the bundle
    /// path. The command is a tiny self-cleaning wrapper script rather than a
    /// symlink: deleting a macOS app runs no uninstall hooks, so a symlink
    /// would dangle forever, while the wrapper notices the app is gone on its
    /// next use and removes itself. Runs on every launch, which rewrites the
    /// wrapper after the app is moved or reinstalled. Best-effort: no admin
    /// prompt — if no candidate directory is writable the app just runs
    /// without a shell command. Skipped for non-bundled dev builds, whose
    /// `target/` path would go stale.
    fn install_cli_command() {
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        if app_bundle_path().is_none() {
            tracing::debug!("Not running from an .app bundle; skipping CLI command install");
            return;
        }
        let script = wrapper_script(&exe);
        for dir in CLI_COMMAND_DIRS {
            match try_install_command_in(std::path::Path::new(dir), &script) {
                Ok(CommandOutcome::Installed) => {
                    tracing::info!("Installed shell command {dir}/{CLI_NAME}");
                    return;
                }
                Ok(CommandOutcome::AlreadyInstalled) => return,
                Ok(CommandOutcome::Foreign) => {
                    // Don't fight the user's setup, and don't install a
                    // second command elsewhere that PATH order may shadow.
                    tracing::info!(
                        "Not installing the {CLI_NAME} shell command: {dir}/{CLI_NAME} \
                         exists and is not this app's wrapper"
                    );
                    return;
                }
                Ok(CommandOutcome::Unavailable) => continue,
                Err(e) => {
                    tracing::debug!("Could not install the CLI command in {dir}: {e}");
                }
            }
        }
        tracing::debug!("No writable PATH directory for the CLI command");
    }

    /// The wrapper script: exec the bundle binary, or self-delete when the
    /// app has been uninstalled (the only cleanup opportunity macOS gives us).
    fn wrapper_script(exe: &std::path::Path) -> String {
        format!(
            "#!/bin/sh\n\
             # {CLI_MARKER}; safe to delete\n\
             APP={app}\n\
             if [ ! -x \"$APP\" ]; then\n\
             \techo \"ESPHome Device Builder is not installed; removing this command.\" >&2\n\
             \trm -f -- \"$0\"\n\
             \texit 127\n\
             fi\n\
             exec \"$APP\" \"$@\"\n",
            app = sh_single_quote(exe)
        )
    }

    /// Single-quote a path for embedding in a shell script, so spaces or
    /// metacharacters in the bundle path can't break or inject anything.
    fn sh_single_quote(path: &std::path::Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
    }

    /// Try to place the wrapper in one directory.
    ///
    /// Replaces only what is safely ours: a marker-bearing wrapper (rewritten
    /// when its content is outdated) or a dangling symlink. A regular file
    /// without the marker, or a live symlink (a user's own arrangement that
    /// works), is treated as foreign and left untouched.
    fn try_install_command_in(
        dir: &std::path::Path,
        script: &str,
    ) -> std::io::Result<CommandOutcome> {
        use std::os::unix::fs::PermissionsExt;

        if !dir.is_dir() {
            return Ok(CommandOutcome::Unavailable);
        }
        let path = dir.join(CLI_NAME);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                if path.exists() {
                    return Ok(CommandOutcome::Foreign);
                }
                // Dangling link: litter nothing can use; reclaim the name.
                std::fs::remove_file(&path)?;
            }
            Ok(meta) if meta.is_file() => {
                let existing = std::fs::read_to_string(&path).unwrap_or_default();
                if !existing.contains(CLI_MARKER) {
                    return Ok(CommandOutcome::Foreign);
                }
                if existing == script {
                    return Ok(CommandOutcome::AlreadyInstalled);
                }
            }
            Ok(_) => return Ok(CommandOutcome::Foreign),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        // Write-then-rename so a shell exec'ing the command mid-refresh never
        // sees a truncated script. Unlike the final `path`, these dirs can be
        // group-writable (Homebrew's /usr/local/bin, /opt/homebrew/bin — see
        // the note on CLI_COMMAND_DIRS), so a *fixed* staging name would let a
        // co-admin pre-plant it as a symlink that a plain `write` +
        // path-`set_permissions` would follow to clobber or chmod an arbitrary
        // file the app can reach. Defeat that with an unpredictable per-process
        // name plus `create_new` (O_CREAT|O_EXCL), which refuses to open
        // anything already at the name — a symlink included — and by chmod'ing
        // the file descriptor rather than the path.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{CLI_NAME}.{}.{seq}.tmp", std::process::id()));
        let write = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(&tmp)
            .and_then(|mut f| {
                f.write_all(script.as_bytes())?;
                // fchmod on the fd (no symlink follow), widen to the intended
                // 0o755 only after the content is in place.
                f.set_permissions(std::fs::Permissions::from_mode(0o755))
            })
            .and_then(|()| std::fs::rename(&tmp, &path));
        match write {
            Ok(()) => Ok(CommandOutcome::Installed),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                let _ = std::fs::remove_file(&tmp);
                Ok(CommandOutcome::Unavailable)
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e)
            }
        }
    }

    /// Resolve the `.app` bundle directory from the running executable
    /// (`<name>.app/Contents/MacOS/<bin>` -> `<name>.app`).
    fn app_bundle_path() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        // ancestors(): <bin>, MacOS, Contents, <name>.app
        let bundle = exe.ancestors().nth(3)?;
        if bundle.extension().and_then(|e| e.to_str()) == Some("app") {
            Some(bundle.to_path_buf())
        } else {
            None
        }
    }

    /// Spawn a detached watcher that waits for this process to exit, then
    /// relaunches the app through LaunchServices (`open`). We wait first because
    /// `tauri-plugin-single-instance` would make a second instance started while
    /// we're still quitting forward-and-exit instead of taking over. Returns
    /// `false` (so the caller falls back to `restart()`) when the bundle path
    /// can't be resolved or the watcher can't be spawned.
    pub fn spawn_launchservices_relaunch() -> bool {
        let Some(bundle) = app_bundle_path() else {
            tracing::warn!("Could not resolve .app bundle path; falling back to direct restart");
            return false;
        };
        // The watcher blocks reading its stdin, which is the read end of a pipe
        // whose write end this process holds. When we exit, the kernel closes
        // that write end, `cat` sees EOF, and the watcher relaunches — so it
        // unblocks exactly when we terminate, with no PID-reuse race and no poll
        // latency. The bundle path is an argv positional ($1) so the shell never
        // parses it (spaces/metacharacters can't break the relaunch or inject a
        // command). Own process group so the watcher outlives us cleanly and
        // isn't caught by any signal aimed at our group.
        let mut child = match Command::new("/bin/sh")
            .arg("-c")
            .arg(r#"cat >/dev/null; exec /usr/bin/open "$1""#)
            .arg("sh") // $0
            .arg(&bundle) // $1, a positional the shell never parses (also fine if non-UTF-8)
            .stdin(Stdio::piped())
            .process_group(0)
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!("Failed to spawn LaunchServices relaunch watcher: {e}");
                return false;
            }
        };
        // Intentionally leak the pipe's write end so it stays open until this
        // process actually exits; that EOF is what releases the watcher. If we
        // dropped it here the watcher would fire immediately, racing our own
        // still-running instance against single-instance.
        if let Some(stdin) = child.stdin.take() {
            std::mem::forget(stdin);
        }
        true
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::util::unique_temp_dir;
        use std::fs;
        use std::path::Path;

        fn bin_dir(dir: &Path) -> PathBuf {
            let bin = dir.join("bin");
            fs::create_dir_all(&bin).expect("bin dir");
            bin
        }

        #[test]
        fn installs_fresh_wrapper() {
            let dir = unique_temp_dir("fresh");
            let bin = bin_dir(&dir);
            let script = wrapper_script(Path::new(
                "/Applications/Some App.app/Contents/MacOS/esphome-desktop",
            ));

            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Installed)
            ));
            let path = bin.join(CLI_NAME);
            let written = fs::read_to_string(&path).expect("read wrapper");
            assert_eq!(written, script);
            assert!(written.starts_with("#!/bin/sh"));
            assert!(written.contains(CLI_MARKER));
            // The space-bearing path must be embedded quoted.
            assert!(written.contains("'/Applications/Some App.app/Contents/MacOS/esphome-desktop'"));
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).expect("meta").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "wrapper must be executable");

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn current_wrapper_is_a_noop() {
            let dir = unique_temp_dir("noop");
            let bin = bin_dir(&dir);
            let script = wrapper_script(Path::new(
                "/Applications/A.app/Contents/MacOS/esphome-desktop",
            ));
            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Installed)
            ));
            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::AlreadyInstalled)
            ));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn outdated_wrapper_is_rewritten() {
            // The app moved: same marker, different baked path.
            let dir = unique_temp_dir("outdated");
            let bin = bin_dir(&dir);
            let old = wrapper_script(Path::new("/old/App.app/Contents/MacOS/esphome-desktop"));
            let new = wrapper_script(Path::new("/new/App.app/Contents/MacOS/esphome-desktop"));
            assert!(matches!(
                try_install_command_in(&bin, &old),
                Ok(CommandOutcome::Installed)
            ));

            assert!(matches!(
                try_install_command_in(&bin, &new),
                Ok(CommandOutcome::Installed)
            ));
            assert_eq!(fs::read_to_string(bin.join(CLI_NAME)).expect("read"), new);

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn foreign_file_is_left_alone() {
            let dir = unique_temp_dir("foreign_file");
            let bin = bin_dir(&dir);
            fs::write(bin.join(CLI_NAME), "someone else's binary").expect("foreign");
            let script = wrapper_script(Path::new(
                "/Applications/A.app/Contents/MacOS/esphome-desktop",
            ));

            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Foreign)
            ));
            assert_eq!(
                fs::read_to_string(bin.join(CLI_NAME)).expect("read"),
                "someone else's binary"
            );

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn live_symlink_is_left_alone() {
            // A user's own working symlink (e.g. straight to the bundle)
            // already does the job; don't replace it.
            let dir = unique_temp_dir("live_link");
            let bin = bin_dir(&dir);
            let other = dir.join("user-wrapper");
            fs::write(&other, "bin").expect("write target");
            std::os::unix::fs::symlink(&other, bin.join(CLI_NAME)).expect("preinstall");
            let script = wrapper_script(Path::new(
                "/Applications/A.app/Contents/MacOS/esphome-desktop",
            ));

            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Foreign)
            ));
            assert_eq!(fs::read_link(bin.join(CLI_NAME)).expect("link"), other);

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn dangling_symlink_is_replaced() {
            let dir = unique_temp_dir("dangling");
            let bin = bin_dir(&dir);
            std::os::unix::fs::symlink(dir.join("gone"), bin.join(CLI_NAME)).expect("preinstall");
            let script = wrapper_script(Path::new(
                "/Applications/A.app/Contents/MacOS/esphome-desktop",
            ));

            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Installed)
            ));
            assert_eq!(
                fs::read_to_string(bin.join(CLI_NAME)).expect("read"),
                script
            );

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn staging_file_does_not_follow_a_planted_symlink() {
            // These PATH dirs can be group-writable, so a co-admin could try to
            // hijack the staging file: pre-plant the old fixed staging name as a
            // symlink to a file they want the app to clobber/chmod. The install
            // must not follow it — it stages under an unpredictable name.
            use std::os::unix::fs::PermissionsExt;
            let dir = unique_temp_dir("staging_symlink");
            let bin = bin_dir(&dir);
            let victim = dir.join("victim");
            fs::write(&victim, "untouched").expect("victim");
            let victim_mode_before = fs::metadata(&victim).expect("meta").permissions().mode();
            // The name a fixed-staging implementation would use.
            std::os::unix::fs::symlink(&victim, bin.join(format!(".{CLI_NAME}.tmp")))
                .expect("plant");
            let script = wrapper_script(Path::new(
                "/Applications/A.app/Contents/MacOS/esphome-desktop",
            ));

            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Installed)
            ));
            // The wrapper landed, and the planted symlink's target was never
            // written through nor chmod'd.
            assert_eq!(
                fs::read_to_string(bin.join(CLI_NAME)).expect("read"),
                script
            );
            assert_eq!(
                fs::read_to_string(&victim).expect("victim"),
                "untouched",
                "the staging write must not follow the planted symlink"
            );
            assert_eq!(
                fs::metadata(&victim).expect("meta").permissions().mode(),
                victim_mode_before,
                "the staging chmod must not follow the planted symlink"
            );

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn missing_directory_is_unavailable() {
            let dir = unique_temp_dir("missing_dir");
            let script = wrapper_script(Path::new(
                "/Applications/A.app/Contents/MacOS/esphome-desktop",
            ));

            assert!(matches!(
                try_install_command_in(&dir.join("no-such-bin"), &script),
                Ok(CommandOutcome::Unavailable)
            ));

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn wrapper_execs_the_app_when_present() {
            // End to end through /bin/sh: the wrapper must exec the target
            // with its arguments forwarded.
            let dir = unique_temp_dir("exec");
            let bin = bin_dir(&dir);
            // A stand-in "app binary" that proves it ran with our args.
            let app = dir.join("fake app.bin");
            fs::write(&app, "#!/bin/sh\necho \"ran with: $1\"\n").expect("write app");
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&app, fs::Permissions::from_mode(0o755)).expect("chmod app");
            let script = wrapper_script(&app);
            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Installed)
            ));

            let output = std::process::Command::new(bin.join(CLI_NAME))
                .arg("status")
                .output()
                .expect("run wrapper");
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                "ran with: status\n"
            );
            assert!(bin.join(CLI_NAME).exists(), "wrapper must survive success");

            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn wrapper_self_deletes_when_the_app_is_gone() {
            // The uninstall case: app path missing → explain, remove itself,
            // exit 127.
            let dir = unique_temp_dir("self_delete");
            let bin = bin_dir(&dir);
            let script = wrapper_script(&dir.join("Trashed.app/Contents/MacOS/esphome-desktop"));
            assert!(matches!(
                try_install_command_in(&bin, &script),
                Ok(CommandOutcome::Installed)
            ));

            let output = std::process::Command::new(bin.join(CLI_NAME))
                .output()
                .expect("run wrapper");
            assert_eq!(output.status.code(), Some(127));
            assert!(String::from_utf8_lossy(&output.stderr).contains("not installed"));
            assert!(
                !bin.join(CLI_NAME).exists(),
                "wrapper must remove itself once the app is gone"
            );

            let _ = fs::remove_dir_all(&dir);
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

    /// Locate every bundled appindicator library that exists inside an
    /// AppImage's `APPDIR`, in candidate-priority order. The caller attempts
    /// each in turn so a first match that fails to load (wrong arch, broken
    /// symlink, missing transitive dep) does not mask a later one that loads.
    fn find_bundled_appindicators(appdir: &Path) -> Vec<PathBuf> {
        appindicator_candidate_paths(appdir)
            .into_iter()
            .filter(|p| p.exists())
            .collect()
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
        //    resident for the lifetime of the process. We try every existing
        //    candidate so a first match that fails to load does not mask a
        //    later one that would have loaded successfully.
        if let Ok(appdir) = std::env::var("APPDIR") {
            let candidates = find_bundled_appindicators(Path::new(&appdir));
            if candidates.is_empty() {
                debug!("APPDIR set but no bundled appindicator library found");
            }
            for lib_path in candidates {
                match unsafe { libloading::Library::new(&lib_path) } {
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
                }
            }
        }

        false
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::util::unique_temp_dir;
        use std::fs;

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
        fn find_bundled_appindicators_locates_existing_library() {
            let appdir = unique_temp_dir("found");
            let lib_dir = appdir.join("shared/lib");
            fs::create_dir_all(&lib_dir).unwrap();
            let lib = lib_dir.join("libayatana-appindicator3.so.1");
            fs::write(&lib, b"\x7fELF").unwrap();

            assert_eq!(find_bundled_appindicators(&appdir), vec![lib]);

            let _ = fs::remove_dir_all(&appdir);
        }

        #[test]
        fn find_bundled_appindicators_returns_all_existing_in_priority_order() {
            let appdir = unique_temp_dir("multi");
            let shared = appdir.join("shared/lib");
            let usr = appdir.join("usr/lib");
            fs::create_dir_all(&shared).unwrap();
            fs::create_dir_all(&usr).unwrap();
            // A lower-priority soname in shared/lib and the preferred soname in
            // a lower-priority dir, to exercise candidate ordering.
            let shared_legacy = shared.join("libappindicator3.so.1");
            let usr_ayatana = usr.join("libayatana-appindicator3.so.1");
            fs::write(&shared_legacy, b"\x7fELF").unwrap();
            fs::write(&usr_ayatana, b"\x7fELF").unwrap();

            let found = find_bundled_appindicators(&appdir);
            assert!(found.contains(&shared_legacy));
            assert!(found.contains(&usr_ayatana));
            // shared/lib precedes usr/lib in APPDIR_LIB_DIRS.
            assert!(
                found.iter().position(|p| p == &shared_legacy)
                    < found.iter().position(|p| p == &usr_ayatana)
            );

            let _ = fs::remove_dir_all(&appdir);
        }

        #[test]
        fn find_bundled_appindicators_returns_empty_when_absent() {
            let appdir = unique_temp_dir("absent");
            fs::create_dir_all(&appdir).unwrap();

            assert!(find_bundled_appindicators(&appdir).is_empty());

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
    #[cfg(unix)]
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

    #[cfg(unix)]
    fn write_stub_interpreter(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let bin = dir.join("python3");
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_is_usable_false_for_missing_binary() {
        let base = unique_temp_dir("interp-missing");
        let _ = std::fs::remove_dir_all(&base);
        assert!(!interpreter_is_usable(&base.join("python3")));
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_is_usable_true_for_healthy_interpreter() {
        let base = unique_temp_dir("interp-healthy");
        let _ = std::fs::remove_dir_all(&base);
        let bin = write_stub_interpreter(&base, "exit 0");
        // Retry to ride out a transient ETXTBSY ("text file busy"): this test
        // binary is multithreaded, and a concurrent fork in another test can
        // briefly leave the just-written stub open for writing, so the first
        // execve of it can fail even though the interpreter is fine. Linux
        // enforces this; macOS does not, which is why only Linux CI flaked.
        const ATTEMPTS: usize = 20;
        let mut usable = false;
        for attempt in 0..ATTEMPTS {
            if interpreter_is_usable(&bin) {
                usable = true;
                break;
            }
            // Don't sleep after the final attempt: nothing follows it, so it
            // would only delay a genuine failure's assert.
            if attempt + 1 < ATTEMPTS {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        assert!(
            usable,
            "interpreter_is_usable never returned true after {ATTEMPTS} attempts \
             (a real exec failure, not the transient ETXTBSY this retry covers)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn interpreter_is_usable_false_when_imports_fail() {
        // Regression test for the corrupt-stdlib shape: an interpreter
        // whose stdlib is gutted still runs `-c "pass"` cleanly but fails any
        // import with ModuleNotFoundError. The stub mimics that: clean exit
        // for trivial scripts, failure the moment the script imports anything.
        // Such a tree must be judged unusable so the refresh wipes and
        // re-copies immediately instead of deferring launch after launch.
        let base = unique_temp_dir("interp-broken-stdlib");
        let _ = std::fs::remove_dir_all(&base);
        let bin = write_stub_interpreter(
            &base,
            "case \"$2\" in *import*) echo \"ModuleNotFoundError: No module named 'types'\" >&2; exit 1;; esac; exit 0",
        );
        assert!(
            !interpreter_is_usable(&bin),
            "an interpreter that cannot import its stdlib must not count as usable"
        );
        let _ = std::fs::remove_dir_all(&base);
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
