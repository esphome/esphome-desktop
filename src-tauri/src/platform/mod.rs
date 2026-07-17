//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tracing::debug;

mod health;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod process;
mod python_env;

pub use health::{
    clear_repair_count, esphome_config_probe, is_managed_python_tree, may_repair_tree,
    repair_budget_left,
};
#[cfg(target_os = "windows")]
pub use process::{assign_to_kill_on_close_job, send_ctrl_break};
pub use process::{
    configure_daemon_tokio_command, isolate_python_tokio_command, pip_command, run_pip,
    run_python_capture, run_python_capture_stdout,
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

/// Parent directory of the managed Python tree (`<dir>/python`) and its repair
/// counter.
///
/// Tauri's `app_local_data_dir()`: the same directory as [`get_data_dir`] on
/// macOS and Linux, but `%LOCALAPPDATA%\io.esphome.builder\` on Windows. The
/// tree is hundreds of MB of interpreter and packages, all reproducible from
/// the bundle, so it belongs in machine-local data where a roaming profile
/// never syncs it. Settings and logs stay under [`get_data_dir`].
pub fn get_python_parent_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let path = app_handle
        .path()
        .app_local_data_dir()
        .context("Failed to get app local data directory")?;

    // Ensure directory exists
    std::fs::create_dir_all(&path).context("Failed to create local data directory")?;

    debug!("Python parent directory: {:?}", path);
    Ok(path)
}

/// Name of the managed Python tree's directory under
/// [`get_python_parent_dir`]. One spelling, because it also appears in the
/// user-facing "delete this folder" repair hint (`update::repair_hint`), where
/// a drift would print an instruction pointing at nothing.
pub const PYTHON_TREE_DIRNAME: &str = "python";

/// Resolve the root of a managed Python tree from its interpreter path.
///
/// `<root>/python.exe` on Windows, `<root>/bin/python3` elsewhere. Deriving the
/// root from the interpreter rather than rebuilding it from the data dir keeps
/// this correct for whichever tree [`get_python_path`] actually selected.
fn python_tree_root(python_bin: &Path) -> Option<&Path> {
    let bin_dir = python_bin.parent()?;
    let root = if cfg!(target_os = "windows") {
        bin_dir
    } else {
        bin_dir.parent()?
    };
    // `get_python_path` falls back to a bare `python3`/`python` for development
    // builds with no bundle. That resolves to an empty root, i.e. the current
    // directory, which is not a managed tree and must not be wiped or marked.
    if root.as_os_str().is_empty() {
        return None;
    }
    Some(root)
}

/// The interpreter path inside a Python tree laid out the way the real bundle
/// is on this platform: [`python_tree_root`]'s inverse. One spelling of the
/// shipped layout, shared between the path resolvers and the tests so a second
/// copy cannot drift from it.
fn interpreter_in_tree(root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        root.join("python.exe")
    } else {
        root.join("bin").join("python3")
    }
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

/// Root of the pristine Python tree inside the bundled resources.
///
/// The `"python"` here is the resource name listed in `tauri.conf.json`'s
/// `bundle.resources`, deliberately NOT [`PYTHON_TREE_DIRNAME`]: renaming the
/// managed tree in app data must not change where the shipped bundle is
/// looked up, and vice versa.
fn get_bundled_python_root(app_handle: &AppHandle) -> Result<PathBuf> {
    Ok(get_bundled_resource_dir(app_handle)?.join("python"))
}

/// Get the path to the user Python executable: the copy `ensure_user_python`
/// keeps under [`get_python_parent_dir`], falling back to the bundled tree
/// before the first-run copy exists and to a bare system Python in development
/// builds with no bundle.
pub fn get_python_path(app_handle: &AppHandle) -> Result<PathBuf> {
    let parent_dir = get_python_parent_dir(app_handle)?;
    let python_path = interpreter_in_tree(&parent_dir.join(PYTHON_TREE_DIRNAME));

    if python_path.exists() {
        debug!("Using user Python: {:?}", python_path);
        return Ok(python_path);
    }

    // Fall back to bundled Python (will be copied on first run)
    let bundled_python = interpreter_in_tree(&get_bundled_python_root(app_handle)?);

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

/// Directory holding the interpreter inside a managed tree. Derived from
/// [`interpreter_in_tree`] so the layout stays spelled once.
fn bin_dir_in_tree(root: &Path) -> PathBuf {
    interpreter_in_tree(root)
        .parent()
        .expect("interpreter_in_tree always returns a path with a parent")
        .to_path_buf()
}

/// Get the Python bin directory (for PATH)
pub fn get_python_bin(app_handle: &AppHandle) -> Result<PathBuf> {
    let parent_dir = get_python_parent_dir(app_handle)?;
    let bin_dir = bin_dir_in_tree(&parent_dir.join(PYTHON_TREE_DIRNAME));

    // If user Python exists, use it
    if bin_dir.exists() {
        return Ok(bin_dir);
    }

    // Fall back to bundled Python
    Ok(bin_dir_in_tree(&get_bundled_python_root(app_handle)?))
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

/// MinGit's CA-bundle locations under the `git` resource dir, as path
/// components in the order MinGit's own `etc/gitconfig` and layout prefer.
/// `prepare_bundle.sh` extracts the MinGit tree whole, so one is always shipped.
///
/// Stored as components, not a `/`-joined literal, so [`first_existing_ca_bundle`]
/// can join them onto the resource dir with the native separator. On Windows the
/// resource dir is a backslash path (`C:\...\git`); a `/`-joined literal would
/// yield a mixed `C:\...\git\mingw64/etc/...`, and the value ends up in
/// `GIT_SSL_CAINFO`, so it must be a clean native path git can consume.
const GIT_CA_BUNDLE_RELATIVE: [&[&str]; 2] = [
    &["mingw64", "etc", "ssl", "certs", "ca-bundle.crt"],
    &["mingw64", "ssl", "certs", "ca-bundle.crt"],
];

/// First of MinGit's CA-bundle locations that exists as a regular file under
/// `git_dir`.
///
/// `is_file`, not `exists`: the result is pinned into `GIT_SSL_CAINFO`, and a
/// directory (or other non-file) at that path would be a value MinGit's OpenSSL
/// backend can't load, so it must not be treated as a usable bundle.
///
/// Split out from [`bundled_git_ca_bundle`] so the candidate order can be
/// unit-tested without a Tauri `AppHandle` or a real bundle on disk, the same
/// split-the-logic pattern [`path_with_prepended`] uses.
// Reached outside tests only through bundled_git_ca_bundle, which is Windows only.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn first_existing_ca_bundle(git_dir: &Path) -> Option<PathBuf> {
    GIT_CA_BUNDLE_RELATIVE
        .iter()
        .map(|components| git_dir.join(components.iter().collect::<PathBuf>()))
        .find(|candidate| candidate.is_file())
}

/// The bundled MinGit CA bundle, if present.
///
/// MinGit ships a CA bundle at `mingw64/etc/ssl/certs/ca-bundle.crt` (with a
/// duplicate at `mingw64/ssl/certs/ca-bundle.crt`). [`ensure_git_on_path`] pins
/// `GIT_SSL_CAINFO` at it so HTTPS clones validate against the bundled bundle
/// instead of whatever `http.sslCAInfo` the ambient git config names (#350).
#[cfg(target_os = "windows")]
fn bundled_git_ca_bundle(app_handle: &AppHandle) -> Result<Option<PathBuf>> {
    let resource_dir = get_bundled_resource_dir(app_handle)?;
    Ok(first_existing_ca_bundle(&resource_dir.join("git")))
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
/// It also pins `GIT_SSL_CAINFO` at MinGit's own bundled CA bundle so HTTPS
/// clones don't depend on the ambient git SSL configuration (issue #350),
/// inherited by the daemon the same way `PATH` is.
///
/// No-op on macOS (the Command Line Tools prompt covers a missing git) and
/// Linux (git ships on all but the most minimal installs).
pub fn ensure_git_on_path(app_handle: &AppHandle) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        use tracing::{info, warn};

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

        // Pin GIT_SSL_CAINFO to MinGit's own bundled CA bundle so HTTPS clones
        // validate against it rather than whatever `http.sslCAInfo` the ambient
        // git config happens to name (issue #350). A machine-wide config left
        // by a previously-installed, since-removed Git for Windows survives the
        // uninstall and can point sslCAInfo at a `C:/Program Files/Git/...`
        // ca-bundle.crt that no longer exists; MinGit's OpenSSL backend then
        // fails every fetch with "error adding trust anchors from file". The env
        // var overrides every config file, so the bundled git always finds the
        // bundled bundle.
        //
        // Only set it when it is not already in the environment: the #350
        // breakage lives in git *config files*, which an unset env var doesn't
        // come from, so this still fixes it, while an explicit GIT_SSL_CAINFO
        // from the user or launcher (e.g. a corporate CA) is left untouched.
        // Log-and-continue if the bundle is somehow absent; an ambient config
        // may still work.
        if std::env::var_os("GIT_SSL_CAINFO").is_some() {
            info!("GIT_SSL_CAINFO already set in the environment; leaving it in place");
        } else {
            match bundled_git_ca_bundle(app_handle)? {
                Some(ca_bundle) => {
                    std::env::set_var("GIT_SSL_CAINFO", &ca_bundle);
                    info!(
                        "Pinned GIT_SSL_CAINFO to bundled CA bundle at {:?}",
                        ca_bundle
                    );
                }
                None => warn!(
                    "Bundled MinGit CA bundle not found; HTTPS clones will rely on \
                     the ambient git SSL configuration"
                ),
            }
        }
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
    fn first_existing_ca_bundle_prefers_etc_then_falls_back() {
        let git_dir = unique_temp_dir("ca-bundle");
        // Build the fixtures from the same component lists the code joins, so the
        // test tracks the constant and mirrors the native-separator join.
        let etc = git_dir.join(GIT_CA_BUNDLE_RELATIVE[0].iter().collect::<PathBuf>());
        let plain = git_dir.join(GIT_CA_BUNDLE_RELATIVE[1].iter().collect::<PathBuf>());

        // Neither present: nothing to point GIT_SSL_CAINFO at.
        assert_eq!(first_existing_ca_bundle(&git_dir), None);

        // Only the non-`etc` variant: fall back to it.
        std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
        std::fs::write(&plain, b"").unwrap();
        assert_eq!(first_existing_ca_bundle(&git_dir), Some(plain.clone()));

        // Both present: the `etc` variant MinGit's own gitconfig names wins.
        std::fs::create_dir_all(etc.parent().unwrap()).unwrap();
        std::fs::write(&etc, b"").unwrap();
        assert_eq!(first_existing_ca_bundle(&git_dir), Some(etc));
    }

    #[test]
    fn first_existing_ca_bundle_ignores_a_directory() {
        // is_file, not exists: a directory sitting where the bundle would be is
        // not a value GIT_SSL_CAINFO can load, so it must be skipped, not pinned.
        let git_dir = unique_temp_dir("ca-bundle-dir");
        let dir_at_etc = git_dir.join(GIT_CA_BUNDLE_RELATIVE[0].iter().collect::<PathBuf>());
        std::fs::create_dir_all(&dir_at_etc).unwrap();
        assert_eq!(first_existing_ca_bundle(&git_dir), None);

        // A real file at the fallback is still picked over the directory.
        let plain = git_dir.join(GIT_CA_BUNDLE_RELATIVE[1].iter().collect::<PathBuf>());
        std::fs::create_dir_all(plain.parent().unwrap()).unwrap();
        std::fs::write(&plain, b"").unwrap();
        assert_eq!(first_existing_ca_bundle(&git_dir), Some(plain));
    }

    /// On Windows the resource dir is a backslash path (`C:\...\git`) and the
    /// result is handed to `GIT_SSL_CAINFO`, so the join must come back a clean
    /// native path, not a mixed `C:\...\git\mingw64/etc/...` one. `unique_temp_dir`
    /// gives a real backslash base here, exercising exactly that join; a
    /// `/`-joined candidate literal would fail the tail assertion. Runs only in
    /// the `windows-latest` CI job (lint-test-cross), the sole place Windows-gated
    /// code is compiled and tested.
    #[cfg(windows)]
    #[test]
    fn first_existing_ca_bundle_yields_native_windows_path() {
        let git_dir = unique_temp_dir("ca-bundle-native");
        let etc = git_dir.join(GIT_CA_BUNDLE_RELATIVE[0].iter().collect::<PathBuf>());
        std::fs::create_dir_all(etc.parent().unwrap()).unwrap();
        std::fs::write(&etc, b"").unwrap();

        let found =
            first_existing_ca_bundle(&git_dir).expect("bundle resolves under a backslash base");
        assert!(
            found
                .to_str()
                .unwrap()
                .ends_with(r"mingw64\etc\ssl\certs\ca-bundle.crt"),
            "GIT_SSL_CAINFO must use native separators, got {found:?}"
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
    /// (#318), and every assertion in the e2e must report on the tree under
    /// test alone.
    fn e2e_run(python: &Path, args: &[&str]) -> (bool, String) {
        let output = run_python_capture(python, args)
            .unwrap_or_else(|e| panic!("failed to run {python:?} {args:?}: {e}"));
        e2e_verdict(output)
    }

    /// [`e2e_run`] with the pip env isolation production pip calls layer on
    /// top ([`process::isolate_pip_command`]). The interpreter isolation alone
    /// is not enough for a pip invocation: an ambient `PIP_REQUIRE_VIRTUALENV`
    /// would fail the install, and a `PIP_TARGET`/`PIP_PREFIX` would aim it
    /// outside the tree under test.
    fn e2e_pip(python: &Path, args: &[&str]) -> (bool, String) {
        let mut cmd = process::python_command(python, args);
        process::isolate_pip_command(&mut cmd);
        let output = cmd
            .output()
            .unwrap_or_else(|e| panic!("failed to run {python:?} {args:?}: {e}"));
        e2e_verdict(output)
    }

    /// Collapse a finished child to (success, stdout+stderr).
    fn e2e_verdict(output: std::process::Output) -> (bool, String) {
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

    /// The whole repair lifecycle against a real bundled Python tree: the
    /// first-run copy, detect the orphan, wipe and re-copy from the pristine
    /// bundle, prove it is fixed, then prove a refresh from an older bundle
    /// restores the newer version the user tree already had.
    ///
    /// Ignored by default because it needs the genuine article — the
    /// python-build-standalone tree with esphome in it that
    /// `build-scripts/prepare_bundle.sh` produces. The `Python tree repair
    /// (e2e)` workflow builds that tree on every OS we ship and runs this with
    /// `--ignored`.
    ///
    /// A venv would not do: on Windows a venv puts `python.exe` in `Scripts/`,
    /// while the real bundle has it at the tree root, so the platform layout in
    /// [`interpreter_in_tree`]/[`python_tree_root`] would go untested against
    /// what we actually ship. This uses the shipped layout.
    ///
    /// The final leg forces the pinned-version restore through a real
    /// `pip install` (#353). Nothing newer than the tree the workflow just
    /// built exists on PyPI to pin the user tree to, so the version asymmetry
    /// is built from the other side: a second bundle source, copied from the
    /// first and pip-downgraded, so the snapshot beats the incoming bundle and
    /// the restore must reinstall. The repair leg itself stays offline; the
    /// downgrade and the restore are where this test reaches PyPI.
    ///
    /// One test rather than several because each step depends on the last
    /// leaving the tree in a particular state, and Rust does not order tests.
    #[test]
    #[ignore = "needs a real bundled Python tree; run by the python-tree-repair CI job"]
    fn e2e_repair_cycle() {
        let bundle = PathBuf::from(std::env::var(E2E_TREE_ENV).unwrap_or_else(|_| {
            panic!("{E2E_TREE_ENV} must point at a tree built by prepare_bundle.sh")
        }));
        // One spelling of the shipped layout, shared with the production path:
        // a second copy here could drift from it, and this is the test that
        // would have to catch that drift.
        let bundled_python = interpreter_in_tree(&bundle);
        assert!(
            bundled_python.is_file(),
            "no interpreter at {bundled_python:?}"
        );
        assert_eq!(
            python_tree_root(&bundled_python),
            Some(bundle.as_path()),
            "python_tree_root disagrees with the shipped layout"
        );

        // 1. The first-run copy, through the real code path. From here on the
        //    bundle is only ever read: it is the pristine source the repair
        //    depends on, exactly as the shipped resource dir is.
        let base = unique_temp_dir("e2e-user-tree");
        let user_tree = base.join("python");
        python_env::refresh_python_tree(&user_tree, || Ok(bundle.clone()), RefreshReason::Startup)
            .expect("first-run copy failed");
        let python = interpreter_in_tree(&user_tree);
        assert!(
            python.is_file(),
            "the copy left no interpreter at {python:?}"
        );
        assert_eq!(
            std::fs::read_to_string(user_tree.join(python_env::PYTHON_VERSION_MARKER))
                .expect("the copy wrote no version marker")
                .trim(),
            env!("CARGO_PKG_VERSION"),
            "the marker must record the version that made the copy"
        );

        // 2. The copy must answer for itself. Its own sysconfig resolving into
        //    the source tree would mean every assertion below green-lights the
        //    bundle instead of the copy under test. Canonicalized on both
        //    sides: macOS reports the tree through /private/var while the temp
        //    path is spelled /var, and that symlink spread must not read as
        //    "outside the tree".
        let purelib = e2e_purelib(&python);
        let canonical_tree = user_tree.canonicalize().unwrap();
        assert!(
            purelib.canonicalize().unwrap().starts_with(&canonical_tree),
            "the copy's purelib {purelib:?} is outside {user_tree:?}, so the copy \
             is not self-contained"
        );
        let (ok, out) = e2e_run(&python, &["-m", "pip", "--version"]);
        assert!(ok, "pip does not run from the copy: {out}");

        // 3. A fresh copy is healthy.
        assert_eq!(
            esphome_config_probe(&python).expect("probe could not run"),
            None,
            "a fresh copy of the bundle must pass the health probe"
        );

        // 4. Orphan a component directory exactly the way --ignore-installed
        //    did: `rp2` declares `rp2040` as a legacy alias, so a leftover
        //    `rp2040` package from the previous version collides with it.
        let orphan = purelib.join("esphome").join("components").join("rp2040");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("__init__.py"), "").unwrap();

        // 5. The probe must catch it. This is the assertion the whole design
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

        // 6. The repair: wipe the damaged copy and re-copy the pristine
        //    bundle, through the same code path the app uses. No network
        //    involved; the snapshot reads the damaged tree's versions and
        //    the restore compares them against the freshly copied ones.
        python_env::refresh_python_tree(&user_tree, || Ok(bundle.clone()), RefreshReason::Repair)
            .expect("repair failed");

        // 7. Healthy again, orphan gone, and still answering from the copy.
        assert!(!orphan.exists(), "the orphan survived the repair");
        assert_eq!(
            esphome_config_probe(&python).expect("probe could not run"),
            None,
            "the tree is still broken after the repair"
        );
        let (ok, out) = e2e_run(&python, &["-m", "esphome", "version"]);
        assert!(ok, "esphome does not run after the repair: {out}");

        // 8. Build a second bundle source that is older than the user tree:
        //    copy the pristine bundle, then downgrade esphome inside the copy.
        //    `--no-deps` keeps the copy on the current dependency set, so the
        //    restore in step 9 resolves against already-satisfied deps and
        //    fetches exactly one wheel.
        let current = python_env::read_package_version(&python, "esphome")
            .expect("could not probe the user tree's esphome version")
            .expect("esphome missing from the user tree");
        let old_bundle = base.join("old-bundle");
        python_env::copy_dir_recursive(&bundle, &old_bundle)
            .expect("could not copy the bundle to a second source");
        let old_python = interpreter_in_tree(&old_bundle);
        let downgrade_spec = format!("esphome<{current}");
        let (ok, out) = e2e_pip(
            &old_python,
            &["-m", "pip", "install", "--no-deps", downgrade_spec.as_str()],
        );
        assert!(
            ok,
            "could not downgrade esphome in the second source: {out}"
        );
        let downgraded = python_env::read_package_version(&old_python, "esphome")
            .expect("could not probe the downgraded source")
            .expect("esphome missing from the downgraded source");
        // The test's premise, checked with the comparator the restore uses.
        assert!(
            crate::update::is_newer_version(&current, &downgraded),
            "the downgrade produced {downgraded}, which is not older than {current}"
        );

        // 9. Refresh from the older source. The snapshot reads the newer
        //    version from the user tree, the copy lands the older one, and the
        //    restore must notice the downgrade and pip-reinstall the newer
        //    from PyPI — the one branch of the refresh the offline legs above
        //    cannot reach.
        python_env::refresh_python_tree(
            &user_tree,
            || Ok(old_bundle.clone()),
            RefreshReason::Repair,
        )
        .expect("refresh from the downgraded source failed");

        // 10. No silent downgrade. The restore is deliberately best-effort in
        //     production (a failed pip install only warns and keeps the
        //     bundled version), so this read is the only place a restore
        //     failure can surface.
        let restored = python_env::read_package_version(&python, "esphome")
            .expect("could not probe the user tree after the restore")
            .expect("esphome missing after the restore");
        assert_eq!(
            restored, current,
            "the refresh downgraded esphome; if this reads {downgraded}, the pip \
             reinstall of {current} failed (PyPI unreachable?) and the restore \
             warning above says why"
        );
        assert_eq!(
            esphome_config_probe(&python).expect("probe could not run"),
            None,
            "the tree is broken after the restore"
        );
        let (ok, out) = e2e_run(&python, &["-m", "esphome", "version"]);
        assert!(ok, "esphome does not run after the restore: {out}");
        assert!(
            out.contains(&current),
            "esphome reports a version other than {current} after the restore: {out}"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_system_python_fallback_is_not_a_managed_tree() {
        // `get_python_path` returns a bare command name in dev builds with no
        // bundle. There is no managed tree behind it, so nothing may be wiped,
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
