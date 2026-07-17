//! Lifecycle of the user-writable bundled Python tree: the copy from the
//! read-only bundle, the version marker and deferral bookkeeping that decide
//! when to refresh it, preserving user-pinned package versions across a
//! refresh, and the cheap "does the interpreter run at all" check the repair
//! path uses to aim its diagnosis.

use super::get_bundled_resource_dir;
use super::get_python_parent_dir;
use super::health::{bump_counter, interpreter_in_tree, read_counter, PROBE_TIMEOUT};
use super::process::{
    pip_install_blocking, run_python_capture, run_python_capture_bounded, tail_for_log,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tracing::{debug, info, warn};

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
/// tree is fixed on every platform (#335).
pub fn ensure_user_python(app_handle: &AppHandle, reason: RefreshReason) -> Result<()> {
    let user_python = get_python_parent_dir(app_handle)?.join("python");
    refresh_python_tree(
        &user_python,
        || Ok(get_bundled_resource_dir(app_handle)?.join("python")),
        reason,
    )
}

/// The refresh itself, parameterized on paths so the repair-cycle e2e can drive
/// the real copy, marker, and snapshot/restore code against a scratch tree
/// without an [`AppHandle`]. `bundled_python` resolves the pristine source
/// lazily: an up-to-date tree never needs it, and resolving it can fail (a dev
/// build with no bundled resources) — asking eagerly would fail the no-op case
/// on the strength of a directory it never reads.
pub(super) fn refresh_python_tree(
    user_python: &Path,
    bundled_python: impl FnOnce() -> Result<PathBuf>,
    reason: RefreshReason,
) -> Result<()> {
    let python_check = interpreter_in_tree(user_python);
    let marker_path = user_python.join(PYTHON_VERSION_MARKER);
    let current_version = env!("CARGO_PKG_VERSION");

    let marker_matches = std::fs::read_to_string(&marker_path)
        .map(|s| s.trim() == current_version)
        .unwrap_or(false);

    // A repair refreshes whatever the marker says: it is called because the
    // tree has already been proven broken, and its marker will match
    // whenever the breakage arrived without an app update — which is exactly
    // the #330 case.
    let needs_copy = reason == RefreshReason::Repair || !python_check.exists() || !marker_matches;

    if needs_copy {
        let bundled_python = bundled_python()?;

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
                        if defers < MAX_REFRESH_DEFERS && bump_counter(&defer_marker, defers + 1) {
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
            std::fs::remove_dir_all(user_python)
                .context("Failed to remove stale user Python directory")?;
        }

        info!(
            "Copying bundled Python to user data directory (version {})...",
            current_version
        );

        // Copy the bundled Python to user data. Timed because the cost is
        // platform-lopsided — tens of thousands of small files, each scanned
        // by Defender on Windows — and a slow launch should say where the
        // time went.
        let copy_started = std::time::Instant::now();
        copy_dir_recursive(&bundled_python, user_python)?;
        let copy_elapsed = copy_started.elapsed();

        // Atomic write: a torn marker could read back as a partial version
        // string, mismatching on next launch and re-copying the whole tree.
        crate::util::atomic_write(&marker_path, current_version)
            .context("Failed to write Python version marker")?;

        restore_preserved_versions(&python_check, &preserved);

        info!(
            "User Python ready at {:?} (copied in {:.1?})",
            user_python, copy_elapsed
        );
    } else {
        debug!(
            "User Python already up-to-date (version {})",
            current_version
        );
    }

    Ok(())
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
/// answer that question. [`super::esphome_config_probe`] asks a bigger one —
/// "can this
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

/// Reinstall any preserved package whose pinned version is newer than the
/// version that just shipped in the new bundled Python tree. Bundled wins
/// for ties and for when bundled is newer (so users always benefit from the
/// app's fresher bundle when they haven't explicitly bumped past it). Each
/// reinstall is best-effort — a network failure here logs a warning and
/// falls through to the bundled version rather than blocking app start.
fn restore_preserved_versions(python_bin: &Path, preserved: &PreservedVersions) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;

    /// A fake bundled tree: a stand-in interpreter file at this platform's
    /// layout (`python.exe` at the root on Windows, `bin/python3` elsewhere)
    /// plus one library file, so a copy has both a nested dir and content to
    /// prove itself with.
    fn fake_bundle(base: &Path) -> PathBuf {
        let bundle = base.join("bundle");
        let interpreter = interpreter_in_tree(&bundle);
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, "stub").unwrap();
        std::fs::write(bundle.join("lib.txt"), "lib").unwrap();
        bundle
    }

    #[test]
    fn first_run_copies_the_bundle_and_writes_the_marker() {
        let base = unique_temp_dir("refresh-first-run");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = base.join("python");

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();

        assert!(
            interpreter_in_tree(&user).is_file(),
            "the interpreter must land at this platform's layout"
        );
        assert_eq!(
            std::fs::read_to_string(user.join("lib.txt")).unwrap(),
            "lib"
        );
        assert_eq!(
            std::fs::read_to_string(user.join(PYTHON_VERSION_MARKER))
                .unwrap()
                .trim(),
            env!("CARGO_PKG_VERSION"),
            "the marker must record the version that made the copy"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn up_to_date_tree_never_resolves_the_bundle() {
        // The bundle resolver can fail in a dev build with no resources; an
        // up-to-date tree must not ask for it, or the routine no-op launch
        // would fail on the strength of a directory it never reads.
        let base = unique_temp_dir("refresh-noop");
        let _ = std::fs::remove_dir_all(&base);
        let user = base.join("python");
        let interpreter = interpreter_in_tree(&user);
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, "stub").unwrap();
        std::fs::write(user.join(PYTHON_VERSION_MARKER), env!("CARGO_PKG_VERSION")).unwrap();
        let sentinel = user.join("sentinel");
        std::fs::write(&sentinel, "").unwrap();

        refresh_python_tree(
            &user,
            || anyhow::bail!("resolved the bundle for an up-to-date tree"),
            RefreshReason::Startup,
        )
        .unwrap();

        assert!(sentinel.exists(), "a matching marker must be a no-op");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn repair_recopies_even_when_the_marker_matches() {
        // The #330 shape: the tree broke without an app update, so the marker
        // still matches. A Repair must refresh anyway — the marker is evidence
        // about versions, and the caller has evidence about damage.
        let base = unique_temp_dir("refresh-repair");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = base.join("python");
        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();
        let orphan = user.join("orphan.txt");
        std::fs::write(&orphan, "damage").unwrap();

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Repair).unwrap();

        assert!(!orphan.exists(), "the repair must wipe before re-copying");
        assert!(interpreter_in_tree(&user).is_file());
        assert_eq!(
            std::fs::read_to_string(user.join(PYTHON_VERSION_MARKER))
                .unwrap()
                .trim(),
            env!("CARGO_PKG_VERSION")
        );

        let _ = std::fs::remove_dir_all(&base);
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
    fn parse_probe_output_reports_version() {
        let v = parse_probe_output("esphome", true, b"2026.5.0\n", b"").unwrap();
        assert_eq!(v, Some("2026.5.0".to_string()));
    }

    #[test]
    fn parse_probe_output_empty_means_absent() {
        let v = parse_probe_output("esphome", true, b"", b"").unwrap();
        assert_eq!(v, None, "clean exit with no output means not installed");
    }

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

    #[test]
    fn refresh_defer_count_missing_marker_is_zero() {
        let base = unique_temp_dir("defer-missing");
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(read_counter(&base.join(".refresh-defer-count")), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

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
