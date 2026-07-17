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

/// Why [`ensure_user_python`] was called. The caller always knows; passing it in
/// keeps one function the single place that decides whether to refresh the tree,
/// and lets that decision differ by intent instead of guessing from the marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    /// A normal launch. Copy on first run or when the app version changed, and
    /// defer if the package versions cannot be read (see [`MAX_REFRESH_DEFERS`]).
    Startup,
    /// A user migrating off the removed classic dashboard backend. Never defers:
    /// the daemon now always launches `esphome_device_builder`, and an old
    /// classic tree may not have it.
    ClassicMigration,
    /// The tree is known broken (#330). Refresh unconditionally: the marker is
    /// beside the point, and deferring would leave a tree we have already proven
    /// cannot build.
    Repair,
}

/// Ensure the user Python exists by copying from bundled Python if needed.
///
/// A version marker file is written into the user Python directory after the
/// copy. On subsequent runs, if the marker is missing or doesn't match the
/// current desktop-app version, the directory is wiped and re-copied so that
/// updated app releases ship a fresh Python tree (e.g. new ESPHome version,
/// changed dependencies). Without this, the first-run copy persisted forever.
///
/// [`RefreshReason::Repair`] additionally forces the copy, which is how a broken
/// tree is fixed on the platforms that keep a pristine bundle.
pub fn ensure_user_python(app_handle: &AppHandle, reason: RefreshReason) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        // Windows keeps no copy: the backend runs straight out of the install
        // dir. Returning Ok to a caller asking for a repair would report a fix
        // that never happened, so say so instead. `can_refresh_from_bundle`
        // steers callers away from here, and this makes the silent no-op
        // impossible rather than merely unlikely.
        if reason == RefreshReason::Repair {
            anyhow::bail!(
                "Windows has no bundled copy to refresh from; the backend runs out of the install dir"
            );
        }
        let resource_dir = get_bundled_resource_dir(app_handle)?;
        let bundled_python = resource_dir.join("python").join("python.exe");

        if !bundled_python.exists() {
            anyhow::bail!("Bundled Python not found at {:?}", bundled_python);
        }

        Ok(())
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

        // A repair refreshes whatever the marker says: it is called because the
        // tree has already been proven broken, and its marker will match
        // whenever the breakage arrived without an app update — which is exactly
        // the #330 case.
        let needs_copy =
            reason == RefreshReason::Repair || !python_check.exists() || !marker_matches;

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
            // For `ClassicMigration` (a user migrating off the removed classic
            // dashboard), `esphome-device-builder` is left out of the snapshot
            // so the freshly bundled copy always wins and the user lands on the
            // current device builder.
            //
            // If the probe FAILS (as opposed to the package being absent), we
            // cannot tell whether the user pinned a newer version, so wiping
            // the tree now would silently discard it — exactly the downgrade
            // this snapshot exists to prevent. In that case defer the refresh:
            // keep the working tree, log a warning, and retry next launch.
            let preserved = if python_check.exists() {
                match snapshot_preserved_versions(
                    &python_check,
                    reason == RefreshReason::ClassicMigration,
                ) {
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
                        // Only a routine `Startup` may defer, because deferring
                        // answers a question only `Startup` is asking: "is this
                        // refresh worth the risk of discarding a pinned
                        // version?" A `ClassicMigration` must land on the
                        // bundled device builder, and a `Repair` was called
                        // because the tree is already proven broken — keeping it
                        // another launch is the wrong answer to both, and the
                        // caller has no way to tell that its request was
                        // silently dropped.
                        if reason == RefreshReason::Startup && interpreter_is_usable(&python_check)
                        {
                            let defer_marker = user_python.join(PYTHON_REFRESH_DEFER_MARKER);
                            let defers = read_counter(&defer_marker);
                            if defers < MAX_REFRESH_DEFERS
                                && bump_counter(&defer_marker, defers + 1)
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
                        } else if reason != RefreshReason::Startup {
                            warn!(
                                "Could not read existing Python package versions ({e:#}) during a \
                                 {reason:?}; refreshing to the bundled tree anyway."
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

/// Read a persisted attempt counter, returning 0 when the marker is missing or
/// unparseable (treat a damaged counter as a fresh start rather than blocking
/// the self-heal it bounds).
fn read_counter(marker_path: &Path) -> u32 {
    std::fs::read_to_string(marker_path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Persist an attempt counter. Returns `true` if the new count was durably
/// written. A `false` (write failed) means the counter can't advance, so the
/// caller must NOT take the bounded action again — otherwise a persistently
/// unwritable marker would re-introduce the very unbounded loop the counter
/// exists to stop, just triggered by a failed write instead of a failed read.
fn bump_counter(marker_path: &Path, count: u32) -> bool {
    match crate::util::atomic_write(marker_path, count.to_string()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Could not persist counter to {marker_path:?}: {e:#}");
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
/// version-restore path. Five minutes is well over the time needed to upgrade
/// `esphome` on a working connection; bounding it prevents a stalled network
/// from hanging app startup indefinitely.
#[cfg(not(target_os = "windows"))]
const PIP_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Maximum length of pip stderr included in a failure error message. pip's
/// resolver and progress output can run to many kilobytes; the actionable
/// failure reason is almost always at the tail, so we truncate to the last
/// N bytes to keep log lines (and downstream UI surfaces) bounded.
const PIP_STDERR_TAIL_BYTES: usize = 4096;

/// Return `s` trimmed and truncated to the last [`PIP_STDERR_TAIL_BYTES`]
/// bytes, with a marker line if anything was dropped. Backs up to a UTF-8
/// char boundary so the result is always valid `str`.
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

/// How often [`run_bounded`] checks whether the child has exited. Small enough
/// that a deadline fires promptly, large enough that polling costs nothing: even
/// the five-minute pip bound is only a few thousand `try_wait` calls.
const CHILD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How a child bounded by [`run_bounded`] finished.
enum BoundedRun {
    /// It exited on its own, within the deadline.
    Exited(std::process::Output),
    /// It outlived the deadline and was killed. Its stderr survives the kill —
    /// for a hung install that partial output is the only diagnostic there is.
    /// stdout is drained too, since that is what keeps the child off a full pipe,
    /// but not kept: no caller has wanted a killed child's stdout.
    TimedOut { stderr: Vec<u8> },
}

/// Run an already-configured `cmd` to completion, killing it if it outlives
/// `timeout`.
///
/// The caller owns the policy — which interpreter, which isolation, which pipes,
/// and what any of the outcomes mean. This owns the part that is easy to get
/// subtly wrong and expensive to get wrong twice: a child whose output fills a
/// pipe buffer (~64 KiB) blocks on `write` until someone reads the other end, so
/// the pipes must be drained on their own threads or the child outlives the very
/// deadline meant to bound it. The readers exit on their own once the child
/// closes its fds, whether it exited or was killed.
fn run_bounded(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<BoundedRun> {
    use std::io::Read;
    use std::thread::JoinHandle;
    use std::time::Instant;

    fn drain<R: Read + Send + 'static>(handle: Option<R>) -> Option<JoinHandle<Vec<u8>>> {
        handle.map(|mut h| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = h.read_to_end(&mut buf);
                buf
            })
        })
    }
    fn collect(reader: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
        reader.and_then(|t| t.join().ok()).unwrap_or_default()
    }

    let mut child = cmd.spawn()?;
    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(BoundedRun::Exited(std::process::Output {
                status,
                stdout: collect(stdout_reader),
                stderr: collect(stderr_reader),
            }));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            // Join the stdout reader before returning so the thread cannot
            // outlive the call, even though its bytes go nowhere.
            let _ = collect(stdout_reader);
            return Ok(BoundedRun::TimedOut {
                stderr: collect(stderr_reader),
            });
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Synchronously run `pip install <package>==<version>` with a wall-clock
/// timeout. Pinning the exact version lets pip resolve pre-releases without
/// needing `--pre`. On timeout the child is killed and an error is returned;
/// the caller logs a warning and falls back to the bundled version, so a
/// stalled pip can't block app launch.
#[cfg(not(target_os = "windows"))]
fn pip_install_blocking(python_bin: &Path, package: &str, version: &str) -> Result<()> {
    let spec = format!("{}=={}", package, version);
    let mut cmd = python_command(python_bin, ["-m", "pip", "install", &spec]);
    // The builder isolates the interpreter; pip needs its own env off too, and
    // every edit is an idempotent `env`/`env_remove`, so layering is a no-op.
    isolate_pip_command(&mut cmd);
    // stderr only: pip's diagnostics go there, and nothing reads its stdout —
    // which also keeps its resolver output out of the capture buffer entirely.
    cmd.stderr(std::process::Stdio::piped());

    let stderr_tail = |bytes: &[u8]| tail_for_log(&String::from_utf8_lossy(bytes));
    match run_bounded(cmd, PIP_INSTALL_TIMEOUT).context("Failed to run pip install")? {
        BoundedRun::Exited(output) if output.status.success() => Ok(()),
        BoundedRun::Exited(output) => {
            anyhow::bail!(
                "pip install {} failed: {}",
                spec,
                stderr_tail(&output.stderr)
            )
        }
        BoundedRun::TimedOut { stderr } => anyhow::bail!(
            "pip install {} timed out after {:?}; partial stderr: {}",
            spec,
            PIP_INSTALL_TIMEOUT,
            stderr_tail(&stderr)
        ),
    }
}

/// Filename of the manifest listing everything that ships with the interpreter
/// itself. Written into the tree at build time by
/// `build-scripts/prepare_bundle.sh`; read by [`wipe_installed_packages`].
const BASE_MANIFEST: &str = ".base-packages";

/// The parsed [`BASE_MANIFEST`]: which directories the reset cleans out, and
/// which entries inside them belong to Python rather than to us.
#[derive(Debug, Default)]
struct BaseManifest {
    /// Directories to clean, relative to the tree root.
    sweep: Vec<PathBuf>,
    /// Entries to spare, relative to the tree root.
    keep: std::collections::HashSet<PathBuf>,
}

/// Resolve the root of a managed Python tree from its interpreter path.
///
/// `<root>/python.exe` on Windows, `<root>/bin/python3` elsewhere. Deriving the
/// root from the interpreter rather than rebuilding it from the data dir keeps
/// this correct for whichever tree [`get_python_path`] actually selected, which
/// is not the same directory on every platform (on Windows it is the install
/// dir, not app data).
fn python_tree_root(python_bin: &Path) -> Option<&Path> {
    let bin_dir = python_bin.parent()?;
    let root = if cfg!(target_os = "windows") {
        bin_dir
    } else {
        bin_dir.parent()?
    };
    // `get_python_path` falls back to a bare `python3`/`python` for development
    // builds with no bundle. That resolves to an empty root, i.e. the current
    // directory, which is not a managed tree and must not be swept or marked.
    if root.as_os_str().is_empty() {
        return None;
    }
    Some(root)
}

/// Reject a manifest path that is absolute or climbs out of the tree.
///
/// The manifest drives recursive deletion, so a corrupt or hand-edited line
/// (`sweep ../../..`) would otherwise aim `remove_dir_all` at the user's home
/// directory. Paths are relative to the tree root by construction, so anything
/// else is a bug and must fail loudly rather than resolve to somewhere real.
fn manifest_path_is_safe(rel: &Path) -> bool {
    use std::path::Component;
    rel.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        && rel.components().any(|c| matches!(c, Component::Normal(_)))
}

/// Match key for a `site-packages` entry: the distribution name for a
/// `<name>-<version>.dist-info` directory, the entry name unchanged otherwise.
///
/// Comparing versioned metadata dirs by name alone would make the manifest go
/// stale the moment any base package's version moves — most obviously pip's own,
/// since `pip install esphome` runs after the manifest is captured and could
/// bump it. The reset would then not recognise `pip-27.0.dist-info` as pip's,
/// delete it, and leave pip importable but with no `RECORD` — which is exactly
/// the state that makes pip abort with `uninstall-no-record-file`. That is the
/// bug this whole change exists to remove, so the reset must not be able to
/// manufacture it. Match on identity, not version.
fn keep_key(name: &str) -> &str {
    match name.strip_suffix(".dist-info") {
        Some(stem) => stem.split_once('-').map_or(stem, |(dist, _version)| dist),
        None => name,
    }
}

/// Rewrite a relative path's final component to its [`keep_key`].
fn keep_path(rel: &Path) -> PathBuf {
    match rel.file_name().and_then(|n| n.to_str()) {
        Some(name) => rel.with_file_name(keep_key(name)),
        // A non-UTF-8 entry name has no version to normalise away; match it
        // verbatim rather than dropping it from the keep set.
        None => rel.to_path_buf(),
    }
}

/// Parse [`BASE_MANIFEST`] text: `sweep <relpath>` / `keep <relpath>` lines,
/// `#` comments and blank lines ignored.
///
/// Paths use POSIX separators on every platform. `Path` compares and hashes
/// component-wise and treats `/` as a separator on Windows too, so the entries
/// match natively-separated paths without rewriting.
///
/// `keep` paths are stored under their [`keep_key`], so the file stays readable
/// (it names the exact versions that shipped) while matching stays
/// version-independent.
fn parse_base_manifest(text: &str) -> Result<BaseManifest> {
    let mut manifest = BaseManifest::default();

    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (verb, rest) = line
            .split_once(char::is_whitespace)
            .with_context(|| format!("{BASE_MANIFEST} line {}: expected '<verb> <path>'", i + 1))?;
        let rel = PathBuf::from(rest.trim());
        if !manifest_path_is_safe(&rel) {
            anyhow::bail!(
                "{BASE_MANIFEST} line {}: unsafe path {:?}",
                i + 1,
                rel.display()
            );
        }
        match verb {
            "sweep" => manifest.sweep.push(rel),
            "keep" => {
                manifest.keep.insert(keep_path(&rel));
            }
            other => anyhow::bail!("{BASE_MANIFEST} line {}: unknown verb {other:?}", i + 1),
        }
    }

    if manifest.sweep.is_empty() {
        anyhow::bail!("{BASE_MANIFEST} names no directories to sweep; refusing to use it");
    }

    // Every swept directory must spare something. Checking the keep set only
    // globally would accept a manifest that sweeps one directory clean because
    // some *other* directory happened to name entries — and sweeping
    // site-packages clean takes the interpreter's own pip with it, which is the
    // one outcome nothing can come back from. A truncated file fails here too.
    for sweep_rel in &manifest.sweep {
        if !manifest
            .keep
            .iter()
            .any(|keep| keep.parent() == Some(sweep_rel.as_path()))
        {
            anyhow::bail!(
                "{BASE_MANIFEST} sweeps {} but names nothing to keep in it; refusing to use it",
                sweep_rel.display()
            );
        }
    }

    Ok(manifest)
}

/// Whether this platform can repair the managed tree by re-copying the bundle
/// it shipped with, rather than reinstalling from PyPI.
///
/// True on macOS and Linux, where the resource tree is read-only by design (a
/// signed `.app`, a squashfs AppImage mount) and so [`ensure_user_python`] keeps
/// a working copy in the app data dir. That copy is exactly what a repair wants:
/// a known-good tree, already on disk, needing no network.
///
/// False on Windows, which has no second copy — the backend runs straight out of
/// the install dir and `ensure_user_python` returns early — so there is nothing
/// to re-copy from and the repair has to come from PyPI.
///
/// That Windows gap is the only reason the manifest and [`wipe_installed_packages`]
/// exist. Close it (#335 — give Windows the app-data copy) and this function,
/// the whole manifest subsystem, and the PyPI reset all go, leaving
/// `ensure_user_python(.., RefreshReason::Repair)` as the one repair everywhere.
pub fn can_refresh_from_bundle(app_handle: &AppHandle) -> bool {
    if cfg!(target_os = "windows") {
        return false;
    }
    match get_bundled_resource_dir(app_handle) {
        Ok(dir) => dir.join("python").is_dir(),
        Err(e) => {
            // Say why. Otherwise this is indistinguishable from "this platform
            // ships no bundle", and the repair quietly downgrades from a free
            // local copy to a network-dependent PyPI rebuild with nothing in the
            // log to explain the difference.
            tracing::warn!(
                "Cannot locate the bundled Python to repair from ({e:#}); falling back to a reinstall"
            );
            false
        }
    }
}

/// Resolve the interpreter the package reset is allowed to delete from,
/// refusing any tree we do not own.
///
/// [`get_python_path`] answers "what should we run?", and its last two fallbacks
/// are the shipped resource tree and a bare system `python3`. That is right for
/// running, and wrong for deleting. `ensure_user_python` is log-and-continue at
/// startup, so a copy that fails for any transient reason (a full disk) leaves
/// `get_python_path` pointing inside `ESPHome Device Builder.app/Contents/
/// Resources/python` on macOS. Wiping *that* succeeds on an admin account,
/// breaks the bundle's code signature, and turns a transient failure into a
/// mandatory reinstall of the app.
///
/// So the reset resolves its target through here instead: the tree must be one
/// we own and put there ourselves. On macOS and Linux that is only ever the copy
/// in the app data dir. On Windows there is no copy — the backend runs straight
/// out of the install dir, which is an ordinary per-user writable directory we
/// wrote — so the resource tree is a legitimate target there and only there.
pub fn python_path_for_reset(app_handle: &AppHandle) -> Result<PathBuf> {
    let python = get_python_path(app_handle)?;
    let root = python_tree_root(&python)
        .with_context(|| format!("Cannot resolve a Python tree root from {python:?}"))?;

    // The tree we copy to and own, on every platform.
    let user_root = get_data_dir(app_handle)?.join("python");
    let resource_root = get_bundled_resource_dir(app_handle)?.join("python");

    if is_resettable_tree(root, &user_root, &resource_root) {
        return Ok(python);
    }

    anyhow::bail!(
        "Refusing to reset packages in {root:?}: not a Python tree this app owns. \
         The managed tree is missing, so the bundled copy is in use; repairing it \
         would damage the installed app rather than fix anything."
    )
}

/// Whether `root` is a Python tree the package reset may delete from.
///
/// Split out from [`python_path_for_reset`] so the rule itself is testable
/// without a live app: it is the guard standing between a recursive delete and
/// the inside of the installed `.app`.
fn is_resettable_tree(root: &Path, user_root: &Path, resource_root: &Path) -> bool {
    // The copy in the app data dir is ours everywhere.
    if root == user_root {
        return true;
    }
    // On Windows there is no copy: `ensure_user_python` returns early and the
    // backend runs straight out of the install dir, an ordinary per-user
    // writable directory the installer wrote. So the resource tree is the live
    // tree there, and repairing it is the whole point. Everywhere else the
    // resource tree is read-only-by-design — a signed `.app` bundle, or a
    // squashfs AppImage mount — and writing to it is damage, not repair.
    cfg!(target_os = "windows") && root == resource_root
}

/// Delete every package we installed, sparing everything that ships with the
/// interpreter. Returns how many entries were removed.
///
/// Callers must resolve `python_bin` through [`python_path_for_reset`], never
/// [`get_python_path`] directly: this deletes recursively, and the latter falls
/// back to trees we must not touch.
///
/// This is the "wipe" half of the recovery for issue #330. `--ignore-installed`
/// used to be how a broken tree was worked around, but it skips pip's uninstall
/// and so orphans the previous version's files: an `esphome/components/rp2040/`
/// left behind by a 2026.6 -> 2026.7 upgrade is what made every compile fail,
/// and orphaned `.dist-info` dirs did the same to version detection (#190).
/// Deleting our packages outright and reinstalling them leaves nothing behind
/// to orphan.
///
/// Scoped by [`BASE_MANIFEST`] rather than by pip's metadata on purpose: the
/// trees that need repairing are exactly the ones whose metadata is unreliable
/// (a missing `RECORD` is what starts this whole failure mode), and an orphaned
/// directory has no metadata at all to consult. The manifest is captured at
/// build time, when the answer is knowable for certain.
///
/// Only ever touches `site-packages` and the scripts dir, never the interpreter
/// or its DLLs, so it cannot hit the locked-`python.exe` problem that a manual
/// Windows reinstall does. Requires the daemon to be stopped: a running backend
/// holds its own imports open.
pub fn wipe_installed_packages(python_bin: &Path) -> Result<usize> {
    use std::fs;

    let root = python_tree_root(python_bin)
        .with_context(|| format!("Cannot resolve Python tree root from {python_bin:?}"))?;
    let manifest_path = root.join(BASE_MANIFEST);

    // Bail rather than fall back to a guessed keep-list. Deleting on an inferred
    // idea of what belongs to Python risks taking pip with it, and the only
    // trees without a manifest are ones built before it existed.
    let text = fs::read_to_string(&manifest_path)
        .with_context(|| format!("Failed to read {manifest_path:?}"))?;
    let manifest = parse_base_manifest(&text)?;

    let mut removed = 0;
    for sweep_rel in &manifest.sweep {
        let sweep_dir = root.join(sweep_rel);
        let entries = match fs::read_dir(&sweep_dir) {
            Ok(entries) => entries,
            // A sweep dir that isn't there yet is not an error: nothing of ours
            // can be in it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("Failed to read {sweep_dir:?}")),
        };

        for entry in entries {
            let entry = entry.with_context(|| format!("Failed to read entry in {sweep_dir:?}"))?;
            // Normalise both sides the same way, so a base package whose version
            // moved since the manifest was captured is still recognised as the
            // interpreter's. See `keep_key`.
            if manifest
                .keep
                .contains(&keep_path(&sweep_rel.join(entry.file_name())))
            {
                continue;
            }
            let path = entry.path();
            // `file_type` does not follow symlinks, so a link is unlinked rather
            // than having its target recursively deleted.
            let is_dir = entry
                .file_type()
                .with_context(|| format!("Failed to stat {path:?}"))?
                .is_dir();
            let result = if is_dir {
                fs::remove_dir_all(&path)
            } else {
                fs::remove_file(&path)
            };
            result.with_context(|| format!("Failed to remove {path:?}"))?;
            removed += 1;
        }
    }

    Ok(removed)
}

/// Hard upper bound on the health probe. Measured at ~0.2s against a real
/// bundled tree, so this is not a budget — it is the line between "slow" and
/// "never", on a path the user is waiting behind. Without it a wedged
/// interpreter means the backend never starts and nothing says why.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The config the health probe validates. Any valid config does the job; it
/// only has to make ESPHome load its component tree.
const PROBE_CONFIG: &str = "esphome:\n  name: healthprobe\nesp32:\n  board: esp32dev\n";

/// Create a scratch directory for the health probe that we know we created.
///
/// `create_dir` fails rather than succeeding when the path already exists, so a
/// name another user pre-created in the shared, world-writable temp dir — a
/// directory, or a symlink pointing somewhere of theirs — is stepped over
/// instead of adopted and written into. The alternative, removing whatever is
/// already there and recreating it, is what the repo avoids for exactly this
/// reason when caching downloads (`prepare_bundle.sh`). The pid keeps the names
/// short for the common case; the counter is what makes it correct.
fn make_probe_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    for attempt in 0..100u32 {
        let dir = base.join(format!(
            "esphome-desktop-probe-{}-{attempt}",
            std::process::id()
        ));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("Failed to create probe dir {dir:?}")),
        }
    }
    anyhow::bail!("Could not create a probe directory under {base:?}")
}

/// Filename of the counter bounding how many times the health probe may trigger
/// a repair. Lives at `<data_dir>/.repair-count`.
///
/// Beside the Python tree, never inside it. On macOS and Linux the repair *is*
/// `remove_dir_all` of the whole tree, so a counter kept within it would be
/// destroyed by the very repair it exists to bound: every launch would read
/// zero, wipe, re-copy, and do it again forever — exactly the loop
/// [`MAX_REPAIRS`] is here to stop.
const REPAIR_COUNT_MARKER: &str = ".repair-count";

/// Maximum repairs triggered by a failing health probe before giving up.
///
/// The probe reports "something makes a real ESPHome command fail", which is
/// deliberately broader than "a repair will fix it" — ESPHome tightening
/// validation on [`PROBE_CONFIG`], or a full disk, would fail it just as well.
/// Unbounded, that would wipe and rebuild the tree on every single launch. Two
/// covers the real case (one repair fixes it, the next launch probes clean)
/// while turning an unfixable failure into a bounded cost and a loud log.
const MAX_REPAIRS: u32 = 2;

/// Whether a failing health probe is allowed to trigger another repair,
/// recording the attempt if so.
///
/// Takes the data dir rather than the tree so the count survives a repair that
/// replaces the tree wholesale; see [`REPAIR_COUNT_MARKER`]. Nothing resets the
/// budget implicitly — [`clear_repair_count`] does it, once a probe
/// actually passes.
pub fn may_repair_tree(data_dir: &Path) -> bool {
    let marker = data_dir.join(REPAIR_COUNT_MARKER);
    let attempts = read_counter(&marker);
    if attempts >= MAX_REPAIRS {
        return false;
    }
    // Record before acting, not after: a repair that dies partway through must
    // still count, or a crashing repair would retry forever. An unwritable
    // counter can never advance, so treat it as exhausted for the same reason.
    bump_counter(&marker, attempts + 1)
}

/// Whether a *future* launch would still be allowed a repair, without spending
/// anything. [`may_repair_tree`] answers the same question by consuming an
/// attempt, which is the wrong tool for deciding what to tell the user.
///
/// This is what makes "reopening will try again" a claim we can check rather
/// than assume: once the budget is spent, nothing retries until a probe passes.
pub fn repair_budget_left(data_dir: &Path) -> bool {
    read_counter(&data_dir.join(REPAIR_COUNT_MARKER)) < MAX_REPAIRS
}

/// Forget any recorded repair attempts, once the tree is proven healthy.
///
/// A missing marker is the normal case and says nothing. Any other failure does:
/// it pins the counter at [`MAX_REPAIRS`] forever, so a later and
/// perfectly fixable breakage is never repaired and the log only ever claims the
/// budget is spent. That is worth a line, for the same reason [`bump_counter`]
/// reports its own write failures rather than swallowing them.
pub fn clear_repair_count(data_dir: &Path) {
    let marker = data_dir.join(REPAIR_COUNT_MARKER);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "Could not clear the repair counter at {marker:?} ({e}); a future repair may be \
             refused as budget-spent"
        ),
    }
}

/// Whether `python_bin` is a Python tree this app manages, as opposed to the
/// bare `python3`/`python` [`get_python_path`] falls back to in development
/// builds with no bundle.
///
/// The health probe and its repair only make sense for a tree we put there: a
/// system Python failing `esphome config` (because ESPHome simply is not
/// installed in it) is not damage, and no repair of ours would touch it.
pub fn is_managed_python_tree(python_bin: &Path) -> bool {
    python_tree_root(python_bin).is_some()
}

/// Check the ESPHome install by running a real `esphome config` validation.
///
/// `Ok(None)` means healthy, `Ok(Some(output))` means broken in a way that
/// breaks real use, `Err` means the probe could not be run at all (which a
/// package reset cannot fix, since the reset needs this same interpreter).
///
/// Runs the actual CLI rather than inspecting package metadata, because the
/// damage is invisible to metadata. The orphaned `components/rp2040/` directory
/// behind #330 is named by no `RECORD`, carries no `.dist-info`, and leaves
/// `importlib.metadata` reporting a perfectly healthy `esphome 2026.7.0` — while
/// every single compile fails. ESPHome builds its component alias map by
/// AST-scanning the components *directory*, so only code that reads that
/// directory can see the conflict.
///
/// `config` is the cheapest command that gets there: the alias map is built at
/// the top of config validation, and a trivial config validates in ~0.2s.
/// `esphome version` never loads the component tree and reports a broken install
/// as fine.
pub fn esphome_config_probe(python_bin: &Path) -> Result<Option<String>> {
    use std::fs;

    // `esphome config` writes alongside the config it is given, so hand it a
    // directory of its own rather than anything of the user's.
    let dir = make_probe_dir()?;

    let result = (|| {
        let config = dir.join("probe.yaml");
        fs::write(&config, PROBE_CONFIG)
            .with_context(|| format!("Failed to write probe config {config:?}"))?;

        // `-I` matches the other maintenance probes: it keeps user site-packages
        // and PYTHONPATH off sys.path, so the probe can only ever report on the
        // managed tree.
        let output = run_python_capture_bounded(
            python_bin,
            [
                OsStr::new("-I"),
                OsStr::new("-m"),
                OsStr::new("esphome"),
                OsStr::new("config"),
                config.as_os_str(),
            ],
            PROBE_TIMEOUT,
        )
        .context("Failed to run esphome config probe")?;

        if output.status.success() {
            return Ok(None);
        }

        // ESPHome reports validation failures on stdout and stderr depending on
        // the stage, so keep both; the reason is what tells a maintainer why a
        // reset happened.
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stdout = stdout.trim();
        if !stdout.is_empty() {
            if !detail.is_empty() {
                detail.push('\n');
            }
            detail.push_str(stdout);
        }
        Ok(Some(tail_for_log(&detail)))
    })();

    let _ = fs::remove_dir_all(&dir);
    result
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

/// Spawn the given Python interpreter with `args` and capture its output,
/// killing it if it outlives `timeout`.
///
/// The unbounded [`run_python_capture`] is right for callers who are already
/// waiting on something else. It is wrong on the launch path: a child that never
/// exits there means the backend never starts and the tray never says why. This
/// module already draws that line for `pip install` — "bounding it prevents a
/// stalled network from hanging app startup indefinitely" — and the same
/// reasoning applies to anything else we make a user wait behind.
fn run_python_capture_bounded<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    let mut cmd = python_command(python, args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    match run_bounded(cmd, timeout)? {
        BoundedRun::Exited(output) => Ok(output),
        BoundedRun::TimedOut { .. } => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out after {timeout:?}"),
        )),
    }
}

/// The command every Python we spawn is built from: the given interpreter and
/// args, isolated from user site-packages (see [`isolate_python_command`]), with
/// no console window on Windows.
///
/// One home for that setup, so "every Python we spawn is isolated" is a property
/// of the builder rather than something each caller has to remember.
fn python_command<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(python);
    cmd.args(args);
    isolate_python_command(&mut cmd);
    configure_no_window_command(&mut cmd);
    cmd
}

/// Spawn the given Python interpreter with `args`, suppress the console
/// window on Windows, isolate it from user site-packages (see
/// [`isolate_python_command`]), and capture its output. It adds no *flags* of
/// its own (callers pass exactly the flags they need, `-I` included or not),
/// and callers keep their own policy for exit status, logging, and
/// stdout/stderr interpretation.
///
/// Unbounded; see [`run_python_capture_bounded`] for callers on the launch path.
pub fn run_python_capture<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::io::Result<std::process::Output> {
    python_command(python, args).output()
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

/// Env that keeps the managed interpreter on its own tree.
///
/// The bundled Python is a plain (non-venv) install, so `site.py` runs
/// `addusersitepackages()` before `addsitepackages()` and the per-user site
/// directory (`~/.local/lib/pythonX.Y/site-packages`, or
/// `%APPDATA%\Python\PythonXY\site-packages` on Windows) lands on `sys.path`
/// AHEAD of our own `site-packages`. Anyone who has ever run `pip install
/// --user` against a same-minor system Python therefore shadows our pinned
/// dependencies with theirs, and the backend dies at import (#318). The
/// ambient `PYTHON*` vars can redirect the interpreter just as effectively, so
/// drop them too.
///
/// This is an env var rather than a `-s` flag so it also reaches the processes
/// the backend spawns for itself (esptool, PlatformIO, compilers), which run
/// against the same tree and have the same exposure. venvs already ignore user
/// site, so inheriting it costs them nothing.
const PYTHON_ISOLATION_SET: [(&str, &str); 1] = [("PYTHONNOUSERSITE", "1")];

/// Ambient vars that can redirect the interpreter off its own tree just as
/// effectively as user site. See [`PYTHON_ISOLATION_SET`].
const PYTHON_ISOLATION_REMOVE: [&str; 3] = ["PYTHONPATH", "PYTHONHOME", "PYTHONSTARTUP"];

/// Point the managed interpreter at its own tree only, per
/// [`PYTHON_ISOLATION_SET`].
pub fn isolate_python_command(cmd: &mut std::process::Command) {
    for (k, v) in PYTHON_ISOLATION_SET {
        cmd.env(k, v);
    }
    for k in PYTHON_ISOLATION_REMOVE {
        cmd.env_remove(k);
    }
}

/// [`isolate_python_command`] for a tokio::process::Command.
///
/// tokio's `Command` is a `std::process::Command` plus a `kill_on_drop` flag;
/// its env methods forward straight to the inner command, and `spawn` runs that
/// same command. So editing it through `as_std_mut` is what tokio would do
/// anyway, and the two variants cannot drift apart.
pub fn isolate_python_tokio_command(cmd: &mut tokio::process::Command) {
    isolate_python_command(cmd.as_std_mut());
}

/// pip settings that would send an install somewhere other than the managed
/// tree [`PYTHON_ISOLATION_SET`] just pinned the interpreter to.
///
/// Both are load-bearing rather than theoretical. `user` is a common `sudo pip`
/// workaround, and it only ever "worked" here because the install went to user
/// site and user site was importable; with the latter now off, pip aborts the
/// install outright. `require-virtualenv` fails every pip call we make
/// regardless of user site, since the bundled tree is not a venv.
///
/// These are forced to `0` rather than unset because pip resolves config as
/// command line > env > config file. Unsetting only clears the ambient env var
/// and leaves a `user = true` in `~/.config/pip/pip.conf` in force; an explicit
/// `0` overrides the file too. Note this deliberately does not touch
/// `PIP_CONFIG_FILE`: dropping it would discard the rest of the user's pip
/// config (a corporate `index-url`, proxy settings) while still leaving the
/// default config files to be read, so it neutralizes nothing on its own.
const PIP_ISOLATION_SET: [(&str, &str); 2] = [("PIP_USER", "0"), ("PIP_REQUIRE_VIRTUALENV", "0")];

/// pip settings that repoint the install directly. Unlike
/// [`PIP_ISOLATION_SET`], these have no "off" value to force: pip strips empty
/// config values before it applies the override order (`if v` in
/// `ConfigOptionParser._get_ordered_configuration_items`), so `PIP_TARGET=""`
/// never reaches the defaults and the config file wins by fallthrough. Dropping
/// the ambient var is all that is available.
///
/// Known residual gap, deliberately not closed: this only clears the env var,
/// so a `target`/`prefix` in the user's own pip.conf still redirects the
/// install off the managed tree, and an ESPHome update then reports success
/// while landing somewhere this interpreter will never import. The only lever
/// that would neutralize it is pointing `PIP_CONFIG_FILE` at the platform's
/// null device (`/dev/null`, `NUL` on Windows), which throws away the rest of
/// their pip config (see [`PIP_ISOLATION_SET`]); that trade is not worth it for
/// a config this rare, and the gap predates the isolation work. Note pip's docs
/// spell that lever `os.devnull`, meaning the *value* of Python's constant: set
/// literally, it is just a relative path that does not exist, and pip silently
/// falls back to the default config files rather than erroring.
const PIP_ISOLATION_REMOVE: [&str; 2] = ["PIP_TARGET", "PIP_PREFIX"];

/// [`isolate_python_command`] plus the `PIP_*` config that would redirect the
/// install target. For commands running `-m pip`.
pub fn isolate_pip_command(cmd: &mut std::process::Command) {
    isolate_python_command(cmd);
    for (k, v) in PIP_ISOLATION_SET {
        cmd.env(k, v);
    }
    for k in PIP_ISOLATION_REMOVE {
        cmd.env_remove(k);
    }
}

/// [`isolate_pip_command`] for a tokio::process::Command. See
/// [`isolate_python_tokio_command`] on why editing the wrapped command works.
pub fn isolate_pip_tokio_command(cmd: &mut tokio::process::Command) {
    isolate_pip_command(cmd.as_std_mut());
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
/// prefilled with `-m pip install` and the Windows no-window flag, and
/// isolated from the ambient Python/pip environment so the install lands in
/// the managed tree (see [`isolate_pip_command`]). Callers append their own
/// package specs and flags before running it.
pub fn pip_command(python: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(python);
    cmd.args(["-m", "pip", "install"]);
    isolate_pip_tokio_command(&mut cmd);
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

/// Tie a spawned child's lifetime to ours on Windows via a kill-on-close job
/// object. Returns `true` if the child was assigned to the job.
///
/// Every graceful shutdown path we have — `send_ctrl_break`, then
/// `TerminateProcess` as the fallback — only runs when the desktop gets to run
/// code. None of it runs when the NSIS uninstaller force-kills us, when we
/// crash, or when the user ends the task from Task Manager. `kill_on_drop` is
/// no help either: the normal quit path calls `std::process::exit()`, which
/// skips `Drop`. The backend is then orphaned, and because Windows runs the
/// interpreter straight out of the install directory (`ensure_user_python`
/// returns early rather than copying it to app data), that orphan keeps
/// `python.exe` — and every file its compile subtree touches, `git.exe`
/// included — open. A later uninstall or in-place upgrade cannot replace or
/// remove them, which strands the install tree and breaks the next launch.
///
/// A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` closes that gap
/// without needing any cooperation from the dying process: when the last
/// handle to the job goes away, Windows terminates everything in it. The
/// kernel closes our handles however we exit, so this holds for a crash or a
/// force-kill just as much as for a clean quit. Descendants inherit job
/// membership, so the backend's compiler and git children are covered too.
///
/// The job deliberately holds only the daemon child, never the desktop process
/// itself. The updater spawns the NSIS installer as our child and then exits;
/// a job that included us would kill that installer mid-update.
///
/// Nested jobs have been supported since Windows 8, so already being inside
/// someone else's job (a launcher, a test runner) does not defeat this outright
/// the way it would have before, when a second assignment always failed. That
/// is not a guarantee of success: assignment can still fail, for instance if
/// the job hierarchy can't be formed. Hence best-effort, and hence the caller
/// gets told whether it worked rather than being allowed to assume it.
///
/// This is a floor, not a replacement for the graceful path: `stop()` still
/// sends `CTRL_BREAK_EVENT` first and gives the backend its full shutdown
/// window. The job only decides what happens to a child that outlives us.
#[cfg(target_os = "windows")]
pub fn assign_to_kill_on_close_job(process: std::os::windows::io::RawHandle) -> bool {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::System::JobObjects::AssignProcessToJobObject;

    let Some(job) = kill_on_close_job() else {
        return false;
    };

    // SAFETY: `job` is a live job handle owned by the process-wide OnceLock
    // (never closed, see `kill_on_close_job`). `process` is the caller's live
    // child handle, which tokio's `Child` keeps open for us. Assignment does
    // not take ownership of either handle.
    match unsafe { AssignProcessToJobObject(job, HANDLE(process)) } {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Failed to assign backend to kill-on-close job object: {e}");
            false
        }
    }
}

/// Owns the process-wide job handle. `HANDLE` is a raw pointer and so neither
/// `Send` nor `Sync`; a job handle is just a kernel object reference with no
/// thread affinity, so sharing it across threads is sound.
#[cfg(target_os = "windows")]
struct JobHandle(::windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for JobHandle {}
#[cfg(target_os = "windows")]
unsafe impl Sync for JobHandle {}

/// The process-wide kill-on-close job, created on first use.
///
/// The handle is intentionally never closed. Its lifetime *is* the mechanism:
/// the job kills its members when the last handle to it closes, and we want
/// that to happen exactly when our process dies. Leaking it into a `OnceLock`
/// leaves the close to the kernel at process teardown, which is the one moment
/// that fires on every exit path including the ones that never run our code.
///
/// `None` if the job could not be set up; the caller then just loses the
/// backstop and keeps the graceful path.
#[cfg(target_os = "windows")]
fn kill_on_close_job() -> Option<::windows::Win32::Foundation::HANDLE> {
    use ::windows::core::PCWSTR;
    use ::windows::Win32::Foundation::CloseHandle;
    use ::windows::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    static JOB: std::sync::OnceLock<Option<JobHandle>> = std::sync::OnceLock::new();

    JOB.get_or_init(|| {
        // SAFETY: Win32 job-object FFI. The unnamed job is created with default
        // security and is owned solely by this closure; on the error path we
        // close it before returning, so no handle leaks and none escapes except
        // the one we deliberately keep for the process lifetime.
        unsafe {
            let job = match CreateJobObjectW(None, PCWSTR::null()) {
                Ok(job) => job,
                Err(e) => {
                    tracing::warn!("Failed to create job object for the backend: {e}");
                    return None;
                }
            };

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            if let Err(e) = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) {
                tracing::warn!("Failed to set kill-on-close limit on the backend job object: {e}");
                let _ = CloseHandle(job);
                return None;
            }

            Some(JobHandle(job))
        }
    })
    .as_ref()
    .map(|job| job.0)
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
    use super::*;
    use crate::util::unique_temp_dir;

    /// Collect a command's staged env edits into (set, removed). `get_envs`
    /// yields `(key, None)` for a var marked for removal and `(key, Some(v))`
    /// for one that is set.
    fn env_edits(cmd: &std::process::Command) -> (Vec<(String, String)>, Vec<String>) {
        let mut set = Vec::new();
        let mut removed = Vec::new();
        for (k, v) in cmd.get_envs() {
            let k = k.to_string_lossy().into_owned();
            match v {
                Some(v) => set.push((k, v.to_string_lossy().into_owned())),
                None => removed.push(k),
            }
        }
        (set, removed)
    }

    #[test]
    fn isolate_python_command_disables_user_site() {
        let mut cmd = std::process::Command::new("python3");
        isolate_python_command(&mut cmd);
        let (set, _) = env_edits(&cmd);
        assert!(set.contains(&("PYTHONNOUSERSITE".to_string(), "1".to_string())));
    }

    #[test]
    fn isolate_python_command_strips_ambient_python_vars() {
        let mut cmd = std::process::Command::new("python3");
        isolate_python_command(&mut cmd);
        let (_, removed) = env_edits(&cmd);
        for var in ["PYTHONPATH", "PYTHONHOME", "PYTHONSTARTUP"] {
            assert!(removed.contains(&var.to_string()), "{var} not removed");
        }
    }

    /// The tokio variants reach through `as_std_mut`, which holds only because
    /// tokio's `Command` stages env on the very `std::process::Command` it later
    /// spawns. Assert the two variants stage identical env rather than
    /// re-listing the vars, so this fails if that ever stops being true. The
    /// std-side tests above prove the compared value isn't vacuously empty.
    #[test]
    fn isolate_python_tokio_command_matches_std_variant() {
        let mut std_cmd = std::process::Command::new("python3");
        isolate_python_command(&mut std_cmd);
        let mut tokio_cmd = tokio::process::Command::new("python3");
        isolate_python_tokio_command(&mut tokio_cmd);
        assert_eq!(env_edits(&std_cmd), env_edits(tokio_cmd.as_std()));
    }

    /// pip resolves and installs against the same interpreter the backend runs
    /// on, so it has to see the same `sys.path` the backend will, and it must
    /// not be redirected off that tree by ambient `PIP_*` config.
    #[test]
    fn pip_command_is_isolated() {
        let cmd = pip_command(Path::new("python3"));
        let (set, removed) = env_edits(cmd.as_std());
        assert!(set.contains(&("PYTHONNOUSERSITE".to_string(), "1".to_string())));
        assert!(set.contains(&("PIP_USER".to_string(), "0".to_string())));
        assert!(removed.contains(&"PYTHONPATH".to_string()));
    }

    /// pip isolation is a superset of Python isolation: a pip install that
    /// lands outside the managed tree is as broken as an import that resolves
    /// outside it.
    #[test]
    fn isolate_pip_command_covers_python_isolation_too() {
        let mut cmd = std::process::Command::new("python3");
        isolate_pip_command(&mut cmd);
        let (set, removed) = env_edits(&cmd);
        for (k, v) in PYTHON_ISOLATION_SET.iter().chain(&PIP_ISOLATION_SET) {
            assert!(
                set.contains(&(k.to_string(), v.to_string())),
                "{k} not set to {v}"
            );
        }
        for var in PYTHON_ISOLATION_REMOVE.iter().chain(&PIP_ISOLATION_REMOVE) {
            assert!(removed.contains(&var.to_string()), "{var} not removed");
        }
    }

    /// See [`isolate_python_tokio_command_matches_std_variant`].
    #[test]
    fn isolate_pip_tokio_command_matches_std_variant() {
        let mut std_cmd = std::process::Command::new("python3");
        isolate_pip_command(&mut std_cmd);
        let mut tokio_cmd = tokio::process::Command::new("python3");
        isolate_pip_tokio_command(&mut tokio_cmd);
        assert_eq!(env_edits(&std_cmd), env_edits(tokio_cmd.as_std()));
    }

    /// pip's precedence is command line > env > config file, so forcing `0`
    /// beats unsetting: a `user = true` in the user's pip.conf survives the
    /// latter. Pin the values, not just the keys, so a future edit back to
    /// `env_remove` fails here rather than in the field.
    #[test]
    fn pip_isolation_forces_off_rather_than_unsetting() {
        let mut cmd = std::process::Command::new("python3");
        isolate_pip_command(&mut cmd);
        let (set, removed) = env_edits(&cmd);
        for var in ["PIP_USER", "PIP_REQUIRE_VIRTUALENV"] {
            assert!(
                set.contains(&(var.to_string(), "0".to_string())),
                "{var} must be forced to 0, not unset"
            );
            assert!(!removed.contains(&var.to_string()));
        }
    }
    /// PID of `parent`'s child named `exe_name`, via a process snapshot.
    /// Used to reach the grandchild the test needs to assert on.
    ///
    /// Matching on the image name rather than taking the first child: a console
    /// is still allocated even under `CREATE_NO_WINDOW`, so `conhost.exe` can
    /// show up parented alongside the process we actually want.
    #[cfg(target_os = "windows")]
    fn child_pid_named(parent: u32, exe_name: &str) -> Option<u32> {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };

        let mut found = None;

        // SAFETY: the snapshot handle is closed on every exit path, and
        // `entry` is initialized with the `dwSize` the API requires before
        // being handed to Process32FirstW/NextW.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return None;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
            while ok {
                if entry.th32ParentProcessID == parent {
                    // Slice at the first NUL rather than converting the whole
                    // fixed buffer and trimming: `entry` is reused every
                    // iteration and nothing promises the API zero-fills past
                    // the terminator, so a short name landing after a longer
                    // one leaves stale tail bytes ("ping.exe\0er.exe"). Those
                    // survive a trailing-NUL trim and silently fail the match.
                    let len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                    if name.eq_ignore_ascii_case(exe_name) {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                }
                ok = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }

        found
    }

    /// Resume a process spawned with `CREATE_SUSPENDED` by resuming its threads.
    ///
    /// `std::process::Command` hands back only a process handle, so reaching the
    /// initial thread means going through a thread snapshot.
    #[cfg(target_os = "windows")]
    fn resume_process(pid: u32) -> bool {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use ::windows::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        let mut resumed = false;

        // SAFETY: snapshot and thread handles are closed on every exit path;
        // `entry` carries the `dwSize` the API requires before first use.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                return false;
            };
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut ok = Thread32First(snapshot, &mut entry).is_ok();
            while ok {
                if entry.th32OwnerProcessID == pid {
                    if let Ok(thread) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                    {
                        // ResumeThread returns the *previous* suspend count, or
                        // -1 on failure. Only a count of 1 or more is a real
                        // resume: 0 means the thread was already running, which
                        // is precisely the state that would make the caller's
                        // inheritance assertion vacuous rather than wrong, so it
                        // must not count.
                        match ResumeThread(thread) {
                            u32::MAX | 0 => {}
                            _ => resumed = true,
                        }
                        let _ = CloseHandle(thread);
                    }
                }
                ok = Thread32Next(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }

        resumed
    }

    /// Whether `pid` is a member of `job`, and a handle-terminate of it, in one
    /// pass so the test can both assert on a grandchild and clean it up.
    #[cfg(target_os = "windows")]
    fn grandchild_in_job_then_kill(
        pid: u32,
        job: ::windows::Win32::Foundation::HANDLE,
    ) -> Option<bool> {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::JobObjects::IsProcessInJob;
        use ::windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        };

        // SAFETY: the opened handle is closed on every exit path; the BOOL out
        // param is a live local.
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                pid,
            )
            .ok()?;
            let mut in_job = ::windows::core::BOOL(0);
            let queried = IsProcessInJob(handle, Some(job), &mut in_job).is_ok();
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
            queried.then(|| in_job.as_bool())
        }
    }

    /// The wiring half: a real child lands in the job, a descendant of it
    /// inherits membership, and the job carries the limit flag that makes
    /// membership fatal. Without the flag the assignment would still "succeed"
    /// and buy us nothing; without the inheritance the compile-subtree and
    /// `git.exe` story is untrue.
    ///
    /// The other half — that membership actually kills when the owner dies — is
    /// `job_kills_its_member_when_the_owner_is_force_killed`.
    #[cfg(target_os = "windows")]
    #[test]
    fn daemon_child_lands_in_a_kill_on_close_job() {
        use ::windows::Win32::System::JobObjects::{
            IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use ::windows::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;

        let job = kill_on_close_job().expect("job object should be creatable");

        // `cmd.exe` runs `ping` as a *grandchild*: only cmd is assigned to the
        // job, so ping is covered purely by the inheritance this asserts on.
        //
        // CREATE_SUSPENDED is what makes that assertion deterministic. Job
        // membership is only inherited by processes created *after* the parent
        // joins, so if cmd got to run `ping` before the assignment below, ping
        // would legitimately not be in the job and this would fail as a flake
        // that reads like a real inheritance bug. Starting cmd suspended means
        // it cannot spawn anything until we resume it, which is strictly after
        // the assignment. (The production spawn accepts this same race rather
        // than paying for it; see the note in `daemon::start_inner`.)
        //
        // CREATE_NO_WINDOW is folded in here rather than via
        // `configure_no_window_command` because `creation_flags` overwrites
        // rather than accumulates. Without it a local `cargo test` flashes a
        // console per run.
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.args(["/c", "ping", "-n", "30", "127.0.0.1"]);
        cmd.creation_flags((CREATE_NO_WINDOW | CREATE_SUSPENDED).0);
        let mut child = cmd.spawn().expect("failed to spawn test child");

        let assigned = assign_to_kill_on_close_job(child.as_raw_handle());
        let resumed = resume_process(child.id());

        // Poll rather than sleep a fixed guess, and don't hang the suite if
        // ping never appears.
        let mut grandchild = None;
        for _ in 0..100 {
            if let Some(pid) = child_pid_named(child.id(), "ping.exe") {
                grandchild = Some(pid);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Kills the grandchild as it goes; `child.kill()` below reaps only cmd
        // itself, which would otherwise leave ping running for its full 30s.
        let grandchild_in_job = grandchild.and_then(|pid| grandchild_in_job_then_kill(pid, job));

        // SAFETY: both handles are live — `job` is the process-wide job and the
        // child handle is kept open by `child`, which outlives this call.
        let in_job = unsafe {
            let mut in_job = ::windows::core::BOOL(0);
            IsProcessInJob(
                ::windows::Win32::Foundation::HANDLE(child.as_raw_handle()),
                Some(job),
                &mut in_job,
            )
            .expect("IsProcessInJob failed");
            in_job.as_bool()
        };

        // SAFETY: `job` is a live job handle; the out buffer and its declared
        // length match `JobObjectExtendedLimitInformation`.
        let flags = unsafe {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            QueryInformationJobObject(
                Some(job),
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                None,
            )
            .expect("QueryInformationJobObject failed");
            info.BasicLimitInformation.LimitFlags
        };

        let _ = child.kill();
        let _ = child.wait();

        assert!(assigned, "child should have been assigned to the job");
        assert!(
            resumed,
            "suspended child was never resumed; the inheritance assertion below \
             would be vacuous rather than wrong"
        );
        assert!(in_job, "child should be a member of the job");
        assert_eq!(
            grandchild_in_job,
            Some(true),
            "a descendant of the assigned child must inherit job membership; the backend's \
             compilers and git children are only covered because of this"
        );
        assert!(
            flags.0 & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.0 != 0,
            "job must be kill-on-close, otherwise membership does not outlive-protect anything"
        );
    }

    /// Set on the re-executed test binary to put it in helper mode.
    #[cfg(target_os = "windows")]
    const JOB_HELPER_ENV: &str = "ESPHOME_JOB_KILL_HELPER";
    /// How the helper hands its member's PID back to the driver.
    #[cfg(target_os = "windows")]
    const JOB_HELPER_MARKER: &str = "JOB_MEMBER_PID=";
    #[cfg(target_os = "windows")]
    const JOB_HELPER_TEST: &str = "platform::tests::job_kill_helper";

    /// The owner half of `job_kills_its_member_when_the_owner_is_force_killed`.
    ///
    /// `#[ignore]` so a normal `cargo test` never runs it: it is only meaningful
    /// when the driver re-execs this binary with `--ignored --exact` and
    /// `JOB_HELPER_ENV` set, and it deliberately blocks until killed. The env
    /// guard means an `--ignored` run by hand exits immediately rather than
    /// hanging for two minutes.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn job_kill_helper() {
        if std::env::var(JOB_HELPER_ENV).is_err() {
            return;
        }

        // Spawn the member exactly the way `daemon::start_inner` spawns the
        // backend — tokio's Command, `configure_daemon_tokio_command`, and the
        // handle from tokio's `raw_handle()` — so this exercises the real code
        // path rather than a std::process lookalike that happens to agree.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("helper: tokio runtime");
        let child_pid = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("ping.exe");
            cmd.args(["-n", "120", "127.0.0.1"]);
            configure_daemon_tokio_command(&mut cmd);
            let child = cmd.spawn().expect("helper: failed to spawn member");
            let handle = child.raw_handle().expect("helper: member has no handle");
            assert!(
                assign_to_kill_on_close_job(handle),
                "helper: could not assign the member to the job"
            );
            let pid = child.id().expect("helper: member has no pid");
            // Leak the Child: dropping it would let tokio reap or kill the
            // member, and the driver needs it alive until *we* are killed.
            std::mem::forget(child);
            pid
        });

        // Printing the PID is also the driver's signal that assignment is done,
        // so it can't kill us before the member is actually in the job.
        println!("{JOB_HELPER_MARKER}{child_pid}");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        // Block until the driver force-kills us. Bounded so a driver that dies
        // early leaves this to time out rather than wedge CI forever.
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    /// The claim the whole change rests on: when the owning process dies without
    /// running any of its own code, Windows kills the job's members.
    ///
    /// Everything else about this feature is verifiable in-process, but not this
    /// — the owner has to actually die, and it can't be the test process. So the
    /// test binary re-execs itself as the owner, waits for it to report a member
    /// it has assigned, then `TerminateProcess`es it. That is precisely the
    /// shape of the cases this exists for: the NSIS uninstaller force-killing
    /// us, a crash, End Task. No `Drop`, no exit handler, no cooperation.
    ///
    /// A handle to the member is opened *before* the kill, so the PID can't be
    /// recycled underneath us and the wait is on the member itself rather than a
    /// poll for its absence.
    #[cfg(target_os = "windows")]
    #[test]
    fn job_kills_its_member_when_the_owner_is_force_killed() {
        use ::windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use ::windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        use std::io::{BufRead, BufReader};

        let exe = std::env::current_exe().expect("current_exe");
        let mut owner = std::process::Command::new(exe)
            .args(["--ignored", "--exact", JOB_HELPER_TEST, "--nocapture"])
            .env(JOB_HELPER_ENV, "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn the owner helper");

        let stdout = owner.stdout.take().expect("owner stdout");
        let member_pid = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix(JOB_HELPER_MARKER)
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
            });
        let Some(member_pid) = member_pid else {
            let _ = owner.kill();
            panic!("the owner never reported a job member; it likely failed to assign one");
        };

        // SAFETY: `member_pid` was just reported by a live child; the handle is
        // closed on every path below.
        let member = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, member_pid) }
            .expect("could not open the job member");

        // The point of the whole test: kill the owner outright.
        owner.kill().expect("failed to kill the owner");
        let _ = owner.wait();

        // SAFETY: `member` is a live handle we own and close immediately after.
        let waited = unsafe { WaitForSingleObject(member, 15_000) };
        let _ = unsafe { CloseHandle(member) };

        assert_eq!(
            waited, WAIT_OBJECT_0,
            "the job did not kill its member when the owning process was force-killed; \
             the backend would survive the desktop and keep holding the install dir open"
        );
    }

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

    /// site-packages path used by the fake trees below. Its literal spelling
    /// does not matter: the manifest names the directories to sweep, so nothing
    /// in the reset infers this layout.
    const TEST_PURELIB: &str = "lib/python3.13/site-packages";

    /// The interpreter path for a Python tree laid out the way the real bundle
    /// is on this platform, so [`python_tree_root`] resolves back to `root`.
    /// Used by both the fabricated trees below and the real-bundle e2e.
    fn interpreter_in_tree(root: &Path) -> PathBuf {
        if cfg!(target_os = "windows") {
            root.join("python.exe")
        } else {
            root.join("bin").join("python3")
        }
    }

    /// Build a fake Python tree holding the interpreter's own pip plus an
    /// installed esphome, and write `manifest` as its base manifest.
    fn fake_tree(tag: &str, manifest: &str) -> PathBuf {
        let root = unique_temp_dir(tag);
        let purelib = root.join(TEST_PURELIB);
        std::fs::create_dir_all(purelib.join("pip")).unwrap();
        std::fs::create_dir_all(purelib.join("pip-26.1.2.dist-info")).unwrap();
        // The orphan from #330 lives inside the package dir, so removing the
        // package removes it too.
        std::fs::create_dir_all(purelib.join("esphome").join("components").join("rp2040")).unwrap();
        std::fs::create_dir_all(purelib.join("esphome-2026.7.0.dist-info")).unwrap();
        std::fs::write(purelib.join("pip").join("__init__.py"), "").unwrap();

        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for name in ["python3", "pip3", "esphome", "esptool"] {
            std::fs::write(bin.join(name), "").unwrap();
        }

        std::fs::write(root.join(BASE_MANIFEST), manifest).unwrap();
        root
    }

    /// A manifest describing [`fake_tree`]'s Python-owned entries.
    fn fake_manifest() -> String {
        format!(
            "# comment\n\
             sweep {TEST_PURELIB}\n\
             sweep bin\n\
             \n\
             keep {TEST_PURELIB}/pip\n\
             keep {TEST_PURELIB}/pip-26.1.2.dist-info\n\
             keep bin/python3\n\
             keep bin/pip3\n"
        )
    }

    #[test]
    fn wipe_removes_our_packages_and_keeps_pythons_own() {
        let root = fake_tree("wipe-keeps-base", &fake_manifest());
        let purelib = root.join(TEST_PURELIB);

        let removed = wipe_installed_packages(&interpreter_in_tree(&root)).unwrap();

        // esphome + its dist-info, and the esphome/esptool scripts.
        assert_eq!(removed, 4, "removed the wrong number of entries");

        // Everything we installed is gone, including the #330 orphan that lived
        // inside it and that no metadata knew about.
        assert!(!purelib.join("esphome").exists());
        assert!(!purelib.join("esphome-2026.7.0.dist-info").exists());
        assert!(!root.join("bin").join("esphome").exists());
        assert!(!root.join("bin").join("esptool").exists());

        // Python's own packages survive. Wiping pip would leave nothing able to
        // reinstall anything, which is the one unrecoverable outcome here.
        assert!(purelib.join("pip").join("__init__.py").exists());
        assert!(purelib.join("pip-26.1.2.dist-info").exists());
        assert!(root.join("bin").join("python3").exists());
        assert!(root.join("bin").join("pip3").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_keeps_pip_after_its_version_moves() {
        // The manifest is captured before `pip install esphome`, so anything in
        // that dependency tree bumping pip leaves the shipped bundle disagreeing
        // with its own manifest; a user upgrading pip by hand does the same.
        // Matching pip's metadata by exact name would then delete it, leaving
        // pip importable with no RECORD, which is precisely the
        // `uninstall-no-record-file` state this whole change exists to remove.
        let root = fake_tree("wipe-pip-upgraded", &fake_manifest());
        let purelib = root.join(TEST_PURELIB);

        // pip upgrades itself: same package dir, new versioned metadata.
        std::fs::remove_dir_all(purelib.join("pip-26.1.2.dist-info")).unwrap();
        std::fs::create_dir_all(purelib.join("pip-27.0.dist-info")).unwrap();
        std::fs::write(purelib.join("pip-27.0.dist-info").join("RECORD"), "").unwrap();

        wipe_installed_packages(&interpreter_in_tree(&root)).unwrap();

        assert!(
            purelib.join("pip-27.0.dist-info").join("RECORD").exists(),
            "pip's metadata must survive a version bump; deleting it would \
             recreate the missing-RECORD bug"
        );
        assert!(purelib.join("pip").exists());
        // Ours still goes, version notwithstanding.
        assert!(!purelib.join("esphome-2026.7.0.dist-info").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn keep_key_ignores_dist_info_versions_only() {
        // Versioned metadata collapses to the distribution name...
        assert_eq!(keep_key("pip-26.1.2.dist-info"), "pip");
        assert_eq!(keep_key("pip-27.0.dist-info"), "pip");
        assert_eq!(keep_key("setuptools-80.9.0.dist-info"), "setuptools");
        // ...including local/pre-release versions, which contain dashes.
        assert_eq!(keep_key("foo-1.0-beta.dist-info"), "foo");
        // Everything else is matched verbatim. `bin/pip3.13` carries Python's
        // version, not the package's, and is fixed for a given bundle.
        assert_eq!(keep_key("pip"), "pip");
        assert_eq!(keep_key("pip3.13"), "pip3.13");
        assert_eq!(
            keep_key("distutils-precedence.pth"),
            "distutils-precedence.pth"
        );
        // A dist-info with no version at all still yields its name.
        assert_eq!(keep_key("weird.dist-info"), "weird");
    }

    #[test]
    fn wipe_without_a_manifest_deletes_nothing() {
        // A tree built before the manifest existed. Guessing which entries are
        // Python's own risks taking pip with them, so refuse outright.
        let root = fake_tree("wipe-no-manifest", &fake_manifest());
        std::fs::remove_file(root.join(BASE_MANIFEST)).unwrap();

        assert!(wipe_installed_packages(&interpreter_in_tree(&root)).is_err());
        assert!(root.join(TEST_PURELIB).join("esphome").exists());
        assert!(root.join(TEST_PURELIB).join("pip").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_rejects_a_manifest_that_escapes_the_tree() {
        // The manifest aims a recursive delete, so a path climbing out of the
        // tree must fail rather than resolve to somewhere real.
        let root = fake_tree("wipe-escape", "sweep ../../..\nkeep bin/python3\n");

        assert!(wipe_installed_packages(&interpreter_in_tree(&root)).is_err());
        assert!(root.join(TEST_PURELIB).join("esphome").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_rejects_a_manifest_with_nothing_to_keep() {
        // A truncated manifest would otherwise sweep site-packages clean,
        // taking pip with it.
        let root = fake_tree("wipe-empty-keep", &format!("sweep {TEST_PURELIB}\n"));

        assert!(wipe_installed_packages(&interpreter_in_tree(&root)).is_err());
        assert!(root.join(TEST_PURELIB).join("pip").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wipe_tolerates_a_sweep_dir_that_does_not_exist() {
        // Windows has no `bin`; nothing of ours can be in a dir that isn't
        // there, so it is skipped rather than failing the whole reset.
        let root = fake_tree("wipe-missing-sweep", &fake_manifest());
        std::fs::remove_dir_all(root.join("bin")).unwrap();

        let removed = wipe_installed_packages(&interpreter_in_tree(&root)).unwrap();
        assert_eq!(removed, 2, "only the site-packages entries remained to go");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_trees_we_own_may_be_reset() {
        let user_root = Path::new("/data/io.esphome.builder/python");
        let resource_root = Path::new("/Applications/ESPHome.app/Contents/Resources/python");

        // The copy in the app data dir is ours to repair, everywhere.
        assert!(is_resettable_tree(user_root, user_root, resource_root));

        // The shipped resource tree is a legitimate target only on Windows,
        // where it IS the live tree. Everywhere else `get_python_path` only
        // returns it because `ensure_user_python`'s copy failed (it is
        // log-and-continue at startup), and deleting inside a signed `.app` or a
        // read-only AppImage mount turns a transient failure into a reinstall.
        assert_eq!(
            is_resettable_tree(resource_root, user_root, resource_root),
            cfg!(target_os = "windows"),
            "the bundled resource tree is resettable on Windows and nowhere else"
        );

        // Anything else — a system Python, a user's own tree — is never ours.
        assert!(!is_resettable_tree(
            Path::new("/usr/local/lib/python3.13"),
            user_root,
            resource_root
        ));
        assert!(!is_resettable_tree(
            Path::new("/data/io.esphome.builder"),
            user_root,
            resource_root
        ));
    }

    #[test]
    fn parse_base_manifest_reads_the_generated_format() {
        // Pins the contract with write_base_manifest() in
        // build-scripts/prepare_bundle.sh, which is the only writer.
        let manifest = parse_base_manifest(&fake_manifest()).unwrap();
        assert_eq!(
            manifest.sweep,
            vec![PathBuf::from(TEST_PURELIB), PathBuf::from("bin")]
        );
        assert!(manifest.keep.contains(&PathBuf::from("bin/python3")));
        assert!(manifest.keep.contains(&PathBuf::from("bin/pip3")));
        assert!(manifest
            .keep
            .contains(&PathBuf::from(format!("{TEST_PURELIB}/pip"))));
        // Four `keep` lines collapse to three entries: `pip` and
        // `pip-26.1.2.dist-info` share the key `pip`, which is the point — it is
        // what lets pip's metadata survive a version bump.
        assert_eq!(manifest.keep.len(), 3);
    }

    #[test]
    fn parse_base_manifest_rejects_a_sweep_dir_with_nothing_kept() {
        // Checking the keep set only globally would accept this: `bin` names
        // entries, so the manifest looks populated, while site-packages is swept
        // clean — taking the interpreter's own pip with it.
        let err = parse_base_manifest(&format!(
            "sweep {TEST_PURELIB}\nsweep bin\nkeep bin/python3\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains(TEST_PURELIB), "{err}");

        // Both covered is fine.
        assert!(parse_base_manifest(&fake_manifest()).is_ok());
    }

    #[test]
    fn run_python_capture_bounded_kills_a_child_that_will_not_exit() {
        // The probe runs in front of daemon.start(); an unbounded child there
        // means the backend never starts and nothing says why.
        let python = Path::new(TEST_PYTHON);
        let started = std::time::Instant::now();
        let err = run_python_capture_bounded(
            python,
            ["-c", "import time; time.sleep(600)"],
            std::time::Duration::from_millis(300),
        )
        .expect_err("a sleeping child must hit the deadline");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline did not fire promptly: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_python_capture_bounded_returns_output_within_the_deadline() {
        let out = run_python_capture_bounded(
            Path::new(TEST_PYTHON),
            ["-c", "print('hi')"],
            std::time::Duration::from_secs(60),
        )
        .expect("a trivial script must not time out");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    }

    /// Any interpreter will do for the bounded-capture tests: they exercise the
    /// process plumbing, not the bundled tree. Named rather than probed for, so a
    /// host without it fails these tests loudly instead of skipping them — a
    /// timeout test that quietly reports green is worse than no timeout test.
    /// Every platform we build on has `python3` (the Python jobs install it, and
    /// `prepare_bundle.sh` needs one regardless).
    #[cfg(not(target_os = "windows"))]
    const TEST_PYTHON: &str = "python3";
    #[cfg(target_os = "windows")]
    const TEST_PYTHON: &str = "python";

    #[test]
    fn parse_base_manifest_rejects_an_unknown_verb() {
        // A verb we don't understand means the file was written by something
        // that disagrees with us about the format; deleting on that basis is
        // not safe.
        assert!(parse_base_manifest("sweep bin\nkeep bin/python3\ndelete bin/pip3\n").is_err());
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
    fn manifest_path_safety() {
        assert!(manifest_path_is_safe(Path::new("lib/site-packages/pip")));
        assert!(manifest_path_is_safe(Path::new("bin")));
        assert!(!manifest_path_is_safe(Path::new("../escape")));
        assert!(!manifest_path_is_safe(Path::new("bin/../../escape")));
        assert!(!manifest_path_is_safe(Path::new("")));
        #[cfg(unix)]
        assert!(!manifest_path_is_safe(Path::new("/etc")));
        #[cfg(windows)]
        assert!(!manifest_path_is_safe(Path::new(r"C:\Windows")));
    }

    #[test]
    fn probe_dir_is_never_a_directory_we_did_not_create() {
        // The temp dir is shared and world-writable, so a name another user
        // pre-created (a directory of theirs, or a symlink into one) must be
        // stepped over rather than adopted and written into.
        let squatted =
            std::env::temp_dir().join(format!("esphome-desktop-probe-{}-0", std::process::id()));
        let _ = std::fs::remove_dir_all(&squatted);
        std::fs::create_dir_all(&squatted).unwrap();
        std::fs::write(squatted.join("theirs.txt"), "not ours").unwrap();

        let dir = make_probe_dir().unwrap();
        assert_ne!(dir, squatted, "must not adopt a pre-existing directory");
        assert!(dir.is_dir());
        assert!(
            squatted.join("theirs.txt").exists(),
            "must not delete another user's directory to take its name"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&squatted);
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

    #[test]
    fn package_reset_is_bounded() {
        // The probe reports "a real command fails", which is broader than "a
        // repair fixes it". Without a bound, a failure a repair can't fix would
        // rebuild the tree on every single launch, forever.
        let data_dir = unique_temp_dir("reset-bound");

        for attempt in 1..=MAX_REPAIRS {
            assert!(
                may_repair_tree(&data_dir),
                "repair {attempt} should be allowed"
            );
        }
        assert!(
            !may_repair_tree(&data_dir),
            "the budget must run out rather than rebuild forever"
        );

        // A tree that proves healthy starts over, so an unrelated future
        // breakage still gets its full budget.
        clear_repair_count(&data_dir);
        assert!(may_repair_tree(&data_dir));

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn reset_budget_survives_the_repair_it_bounds() {
        // On macOS/Linux the repair is `remove_dir_all` of the whole Python
        // tree. A counter kept inside that tree would be destroyed by the very
        // repair it bounds, so every launch would read zero and rebuild again:
        // the unbounded loop MAX_REPAIRS exists to prevent.
        let data_dir = unique_temp_dir("reset-budget-survives");
        let python_tree = data_dir.join("python");
        std::fs::create_dir_all(python_tree.join("bin")).unwrap();

        assert!(may_repair_tree(&data_dir), "first repair allowed");

        // The repair replaces the tree.
        std::fs::remove_dir_all(&python_tree).unwrap();
        std::fs::create_dir_all(python_tree.join("bin")).unwrap();

        assert!(may_repair_tree(&data_dir), "second repair allowed");
        assert!(
            !may_repair_tree(&data_dir),
            "the budget must not be reset by the repair that spends it"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn refresh_defer_count_missing_marker_is_zero() {
        let base = unique_temp_dir("defer-missing");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(read_counter(&base.join(".refresh-defer-count")), 0);
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

        let mut count = read_counter(&marker);
        let mut defers = 0;
        while count < MAX_REFRESH_DEFERS {
            bump_counter(&marker, count + 1);
            count = read_counter(&marker);
            defers += 1;
        }
        assert_eq!(defers, MAX_REFRESH_DEFERS, "defers are bounded");
        assert_eq!(count, MAX_REFRESH_DEFERS, "counter persists across reads");

        let _ = std::fs::remove_dir_all(&base);
    }
}
