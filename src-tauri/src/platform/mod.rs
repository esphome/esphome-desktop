//! Platform-specific functionality
//!
//! Provides abstractions for platform-specific paths and behaviors.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};
use tracing::debug;

mod env_path;
mod health;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod pip;
mod process;
mod python_env;
#[cfg(target_os = "windows")]
mod windows;

pub use env_path::{ensure_ccache_on_path, ensure_git_on_path, ensure_homebrew_on_path};
pub use health::{
    clear_repair_count, esphome_config_probe, is_managed_python_tree, may_repair_tree,
    repair_budget_left,
};
pub use pip::{pip_command, pip_output_report, run_pip};
#[cfg(target_os = "windows")]
pub use process::{assign_to_kill_on_close_job, send_ctrl_break};
pub use process::{
    configure_daemon_tokio_command, isolate_python_tokio_command, run_python_capture_stdout,
};
pub(crate) use python_env::{dedupe_dist_info, detect_device_builder_version, DistInfoDedupeScope};
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

/// Interpreter path of the managed user Python tree, whether or not the
/// first-run copy exists yet.
///
/// The single spelling of that composition, shared by [`get_python_path`] and
/// the Windows firewall rule, which must scope its program filter to the
/// exact path the daemon executes; a second copy is how the two would drift
/// apart.
fn managed_interpreter_path(app_handle: &AppHandle) -> Result<PathBuf> {
    Ok(interpreter_in_tree(
        &get_python_parent_dir(app_handle)?.join(PYTHON_TREE_DIRNAME),
    ))
}

/// Get the path to the user Python executable: the copy `ensure_user_python`
/// keeps under [`get_python_parent_dir`], falling back to the bundled tree
/// before the first-run copy exists and to a bare system Python in development
/// builds with no bundle.
pub fn get_python_path(app_handle: &AppHandle) -> Result<PathBuf> {
    let python_path = managed_interpreter_path(app_handle)?;

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

/// Platform-specific initialization
#[cfg_attr(target_os = "linux", allow(unused_variables))]
pub fn init(app_handle: &AppHandle) {
    #[cfg(target_os = "macos")]
    macos::init(app_handle);

    #[cfg(target_os = "windows")]
    windows::init(app_handle);

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

    /// Env var naming the real bundled Python tree the e2e test runs against.
    const E2E_TREE_ENV: &str = "ESPHOME_E2E_PYTHON_TREE";

    /// Run the tree's interpreter and return (success, stdout+stderr).
    ///
    /// Goes through [`process::run_python_capture`] so the harness is isolated
    /// exactly as the code under test is. Spawning the interpreter directly
    /// would let the runner's user site-packages or an ambient `PYTHONPATH`
    /// satisfy an import (#318), and every assertion in the e2e must report on
    /// the tree under test alone.
    fn e2e_run(python: &Path, args: &[&str]) -> (bool, String) {
        let output = process::run_python_capture(python, args)
            .unwrap_or_else(|e| panic!("failed to run {python:?} {args:?}: {e}"));
        e2e_verdict(output)
    }

    /// [`e2e_run`] with the pip env isolation production pip calls layer on
    /// top ([`pip::isolate_pip_command`]). The interpreter isolation alone
    /// is not enough for a pip invocation: an ambient `PIP_REQUIRE_VIRTUALENV`
    /// would fail the install, and a `PIP_TARGET`/`PIP_PREFIX` would aim it
    /// outside the tree under test.
    fn e2e_pip(python: &Path, args: &[&str]) -> (bool, String) {
        let mut cmd = process::python_command(python, args);
        pip::isolate_pip_command(&mut cmd);
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
        // Dirty the second source the way the installer overlay does (#389):
        // a stale dist-info stranded beside the real one, so importlib gets
        // two answers for esphome's version. The refresh below must not
        // reproduce this into the user tree.
        let stale_dist_info = e2e_purelib(&old_python).join("esphome-0.0.1.dist-info");
        std::fs::create_dir_all(&stale_dist_info).unwrap();
        std::fs::write(
            stale_dist_info.join("METADATA"),
            "Metadata-Version: 2.1\nName: esphome\nVersion: 0.0.1\n",
        )
        .unwrap();

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

        // 11. The stale dist-info planted in the source must not have survived
        //     the copy: the post-copy dedupe prunes to one dist-info per
        //     package, so importlib has exactly one answer for esphome — the
        //     direct negation of the #389 symptom. The step-2 `purelib` path
        //     is still the right place to look: the refresh recreated the
        //     directory, but at the same location.
        let esphome_dist_infos: Vec<_> = std::fs::read_dir(&purelib)
            .expect("could not list the refreshed purelib")
            .filter_map(|entry| {
                let name = entry.unwrap().file_name().into_string().unwrap();
                (name.starts_with("esphome-") && name.ends_with(".dist-info")).then_some(name)
            })
            .collect();
        assert_eq!(
            esphome_dist_infos,
            [format!("esphome-{current}.dist-info")],
            "the refreshed tree must hold exactly one esphome dist-info"
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
