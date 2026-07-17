//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

#![allow(dead_code)]

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tracing::debug;

mod base_manifest;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod process;

use base_manifest::python_tree_root;
pub use base_manifest::{can_refresh_from_bundle, python_path_for_reset, wipe_installed_packages};
#[cfg(not(target_os = "windows"))]
use process::pip_install_blocking;
#[cfg(target_os = "windows")]
pub use process::{assign_to_kill_on_close_job, send_ctrl_break};
pub use process::{
    configure_daemon_tokio_command, isolate_python_tokio_command, pip_command, run_python_capture,
    run_python_capture_stdout,
};
use process::{run_python_capture_bounded, tail_for_log};

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
                        // A check we could not make is not a check that failed.
                        // Wiping on "we could not tell" would discard the user's
                        // pinned version on the strength of an unanswered
                        // question — the very downgrade the snapshot above
                        // exists to prevent. Assume usable and defer; that is
                        // bounded, so a persistently unanswerable check still
                        // self-heals after MAX_REFRESH_DEFERS.
                        let usable = interpreter_is_usable(&python_check).unwrap_or_else(|probe| {
                            warn!(
                                "Could not check whether the interpreter at {python_check:?} is \
                                 usable ({probe}); assuming it is rather than wiping a tree that \
                                 may be fine"
                            );
                            true
                        });
                        if reason == RefreshReason::Startup && usable {
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
///
/// This asks only about the interpreter, which is what makes it the right way to
/// answer that question. [`esphome_config_probe`] asks a bigger one — "can this
/// tree build?" — and fails for reasons that have nothing to do with the
/// interpreter (an unwritable temp dir, a full disk). Inferring "the interpreter
/// is broken" from *that* failing would condemn a healthy tree.
///
/// Bounded, because both callers are on the launch path: an interpreter wedged
/// rather than broken would otherwise hang the very startup this is meant to
/// rescue.
/// `Err` means the check itself could not be made — the spawn failed for a
/// reason that says nothing about this interpreter (`EMFILE`, `EPERM`), or it
/// outran [`PROBE_TIMEOUT`] on a loaded machine. That is not the same as an
/// interpreter that ran and failed, and callers must not treat it as one:
/// collapsing the two would wipe a working tree, discarding the user's pinned
/// versions, on the strength of a question we never got an answer to.
pub fn interpreter_is_usable(python_bin: &Path) -> std::io::Result<bool> {
    match run_python_capture_bounded(
        python_bin,
        ["-c", "import importlib.metadata"],
        PROBE_TIMEOUT,
    ) {
        Ok(o) => Ok(o.status.success()),
        // An interpreter that is not there is an answer, not a failure to get
        // one: nothing about it will run, now or later.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
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

    // A leaked probe dir is not harmless: `make_probe_dir` steps over names it
    // did not create, and it only tries 100 of them. Enough of these and the
    // probe stops running with "Could not create a probe directory" and nothing
    // in the log tying it back to the cleanup that quietly never happened.
    if let Err(e) = fs::remove_dir_all(&dir) {
        tracing::warn!("Could not remove the probe dir {dir:?}: {e}");
    }
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
    fn parse_probe_output_reports_version() {
        let v = parse_probe_output("esphome", true, b"2026.5.0\n", b"").unwrap();
        assert_eq!(v, Some("2026.5.0".to_string()));
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn parse_probe_output_empty_means_absent() {
        let v = parse_probe_output("esphome", true, b"", b"").unwrap();
        assert_eq!(v, None, "clean exit with no output means not installed");
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
        // A missing interpreter is a definitive "no", not an unanswered question.
        assert!(!interpreter_is_usable(&base.join("python3")).unwrap());
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
            if interpreter_is_usable(&bin).unwrap_or(false) {
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
    fn interpreter_is_usable_separates_a_failed_check_from_a_failed_interpreter() {
        // A check we could not make must not read as an interpreter that failed:
        // callers wipe on the latter, and wiping on the former discards a user's
        // pinned version over a question nobody answered. A directory is not an
        // executable, so spawning it fails with something other than NotFound.
        let base = unique_temp_dir("interp-unanswerable");
        let dir_not_a_binary = base.join("bin");
        std::fs::create_dir_all(&dir_not_a_binary).unwrap();
        assert!(
            interpreter_is_usable(&dir_not_a_binary).is_err(),
            "a spawn that fails for reasons other than absence is an unanswered \
             question, not a verdict"
        );

        // Whereas absence is a verdict: nothing about it will ever run.
        assert!(!interpreter_is_usable(&base.join("nope")).unwrap());

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
            !interpreter_is_usable(&bin).unwrap(),
            "an interpreter that cannot import its stdlib must not count as usable"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The interpreter path for a Python tree laid out the way the real bundle
    /// is on this platform, so [`python_tree_root`] resolves back to `root`.
    /// Used by the real-bundle e2e here and by the fabricated trees in
    /// `base_manifest`'s tests.
    pub(super) fn interpreter_in_tree(root: &Path) -> PathBuf {
        if cfg!(target_os = "windows") {
            root.join("python.exe")
        } else {
            root.join("bin").join("python3")
        }
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
    fn repairs_are_bounded() {
        // The probe reports "a real command fails", which is broader than "a
        // repair fixes it". Without a bound, a failure a repair can't fix would
        // rebuild the tree on every single launch, forever.
        let data_dir = unique_temp_dir("repair-bound");

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
        let data_dir = unique_temp_dir("repair-budget-survives");
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
