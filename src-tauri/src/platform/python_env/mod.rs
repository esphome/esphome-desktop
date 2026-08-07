//! Lifecycle of the user-writable bundled Python tree: the copy from the
//! read-only bundle, the version marker and deferral bookkeeping that decide
//! when to refresh it, and preserving user-pinned package versions across a
//! refresh. The interpreter and package probes it consults live in [`probe`];
//! the tree copy itself in [`copy`].

use super::health::{bump_counter, read_counter};
use super::pip::pip_install_blocking;
use super::{
    get_bundled_python_root, get_python_parent_dir, interpreter_in_tree, PYTHON_TREE_DIRNAME,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tauri::AppHandle;
use tracing::{debug, info, warn};

mod copy;
mod probe;

pub(super) use copy::copy_dir_recursive;
pub use probe::interpreter_is_usable;
pub(super) use probe::read_package_version;
pub(crate) use probe::{dedupe_dist_info, detect_device_builder_version, DistInfoDedupeScope};

/// Filename of the marker recording which desktop-app version copied the
/// user Python tree. Lives at `<user_python>/.esphome-desktop-version`.
pub(super) const PYTHON_VERSION_MARKER: &str = ".esphome-desktop-version";

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
    let user_python = get_python_parent_dir(app_handle)?.join(PYTHON_TREE_DIRNAME);
    refresh_python_tree(&user_python, || get_bundled_python_root(app_handle), reason)
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

    if !needs_copy {
        debug!(
            "User Python already up-to-date (version {})",
            current_version
        );
        return Ok(());
    }

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
        match snapshot_preserved_versions(&python_check, reason == RefreshReason::ClassicMigration)
        {
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
        // Name the actual trigger: this branch also runs for a Repair (marker
        // intact) and for a missing interpreter, and a log that always blames
        // the marker would misdirect exactly the diagnosis a repair log serves.
        info!(
            "Removing user Python at {:?} before re-copying the bundle ({:?} refresh; marker match: {})",
            user_python, reason, marker_matches
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

    // The bundle is not guaranteed clean: the installer overlays the install
    // dir without deleting the previous release's files, so the source can
    // carry both releases' `.dist-info` dirs and the copy above reproduces
    // them (#389). Prune to one dist-info per package before anything reads
    // the fresh tree — the version restore below and every later pip
    // uninstall rely on `importlib.metadata`, which duplicates make
    // ambiguous. Best-effort even though an `Err` can now also mean a
    // RECORD-less dir survived (the shape that aborts installs): the tree
    // still works for everything except that install, whose retry surfaces
    // pip's own report to the user, while failing the refresh here would
    // trade a degraded tree for none. A graceful failure does reach the
    // marker write below, so later launches will not re-prune on their own;
    // the route out is the install that hits the surviving damage, whose
    // missing-RECORD recovery forces this whole path again for any package
    // (the builder-scoped lazy heal also retries its two). Skipping the
    // marker instead would re-copy the full tree on every launch for as long
    // as the prune kept failing. Runs before the marker write so a crash
    // mid-prune leaves no marker and the next launch re-copies and re-prunes.
    if let Err(e) = dedupe_dist_info(&python_check, DistInfoDedupeScope::All) {
        warn!(
            "dist-info dedup after the bundle copy failed ({e:#}); continuing with the copied tree"
        );
    }

    // Atomic write: a torn marker could read back as a partial version
    // string, mismatching on next launch and re-copying the whole tree.
    crate::util::atomic_write(&marker_path, current_version)
        .context("Failed to write Python version marker")?;

    restore_preserved_versions(&python_check, &preserved);

    info!(
        "User Python ready at {:?} (copied in {:.1?})",
        user_python, copy_elapsed
    );

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

/// Test helper: write an executable `python3` shell script into `dir` whose
/// body is `body`, and return its path. Module-level (like
/// [`crate::util::unique_temp_dir`]) so sibling modules' tests can stub an
/// interpreter without re-writing the shebang/chmod recipe.
#[cfg(test)]
#[cfg(unix)]
pub(super) fn write_stub_interpreter(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let bin = dir.join("python3");
    std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

/// Test helper: a stub-interpreter body that appends the last argv entry — the
/// maintenance script's mode argument — to `log`, so a test can assert which
/// mode a spawn used without wading through the embedded script text.
/// Module-level, like [`write_stub_interpreter`], because both this module's
/// tests and [`probe`]'s need it.
#[cfg(test)]
#[cfg(unix)]
pub(super) fn log_last_arg_stub(log: &Path) -> String {
    format!(
        "for a; do last=$a; done; echo \"$last\" >> {}",
        log.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;

    /// Write a stand-in interpreter file into `root` at this platform's layout
    /// (`python.exe` at the root on Windows, `bin/python3` elsewhere).
    fn stub_tree(root: &Path) {
        let interpreter = interpreter_in_tree(root);
        std::fs::create_dir_all(interpreter.parent().unwrap()).unwrap();
        std::fs::write(&interpreter, "stub").unwrap();
    }

    /// A fake bundled tree: a stub interpreter plus one library file, so a
    /// copy has both a nested dir and content to prove itself with.
    fn fake_bundle(base: &Path) -> PathBuf {
        let bundle = base.join("bundle");
        stub_tree(&bundle);
        std::fs::write(bundle.join("lib.txt"), "lib").unwrap();
        bundle
    }

    /// Assert the version marker in `tree` records the running app version.
    fn assert_marker_current(tree: &Path) {
        assert_eq!(
            std::fs::read_to_string(tree.join(PYTHON_VERSION_MARKER))
                .expect("no version marker in the tree")
                .trim(),
            env!("CARGO_PKG_VERSION"),
            "the marker must record the version that made the copy"
        );
    }

    #[test]
    fn first_run_copies_the_bundle_and_writes_the_marker() {
        let base = unique_temp_dir("refresh-first-run");
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
        assert_marker_current(&user);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn up_to_date_tree_never_resolves_the_bundle() {
        // The bundle resolver can fail in a dev build with no resources; an
        // up-to-date tree must not ask for it, or the routine no-op launch
        // would fail on the strength of a directory it never reads.
        let base = unique_temp_dir("refresh-noop");
        let user = base.join("python");
        stub_tree(&user);
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
        let bundle = fake_bundle(&base);
        let user = base.join("python");
        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();
        let orphan = user.join("orphan.txt");
        std::fs::write(&orphan, "damage").unwrap();

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Repair).unwrap();

        assert!(!orphan.exists(), "the repair must wipe before re-copying");
        assert!(interpreter_in_tree(&user).is_file());
        assert_marker_current(&user);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn refresh_copy_runs_the_all_packages_dedupe() {
        // The copied bundle is not guaranteed clean (#389), so every copy must
        // finish with a `dedupe-all` pass through the fresh tree's own
        // interpreter — here a stub that records the mode it was invoked with.
        let base = unique_temp_dir("refresh-dedupe");
        let bundle = base.join("bundle");
        let log = base.join("calls.log");
        write_stub_interpreter(&bundle.join("bin"), &log_last_arg_stub(&log));
        let user = base.join("python");

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();

        let calls = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            calls.lines().any(|line| line == "dedupe-all"),
            "the copy must run the all-packages dist-info dedupe, got: {calls:?}"
        );
        assert_marker_current(&user);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn refresh_survives_a_failing_dedupe() {
        // The dedupe is best-effort: duplicate metadata does not fail the
        // health probe, so a dedupe failure must not fail the refresh either —
        // the marker still lands and the tree stays usable.
        let base = unique_temp_dir("refresh-dedupe-fail");
        let bundle = base.join("bundle");
        write_stub_interpreter(&bundle.join("bin"), "exit 1");
        let user = base.join("python");

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup)
            .expect("a failed dedupe must not fail the refresh");
        assert_marker_current(&user);

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

    /// Stub interpreter body that fails the version probe while passing the
    /// usability check — the "the tree is fine, we just can't read what is
    /// pinned in it" shape the defer exists for. The probe's script is the only
    /// one mentioning `PackageNotFoundError`, so matching argv separates it from
    /// the bare `import importlib.metadata` the usability check runs.
    #[cfg(unix)]
    const PROBE_FAILS_BUT_USABLE: &str =
        "case \"$*\" in *PackageNotFoundError*) exit 1;; esac; exit 0";

    /// A user tree whose interpreter is a stub running `body`, plus a sentinel
    /// file that only survives if the tree is left in place. Every test below
    /// turns on the same question — was this tree wiped? — so they ask it the
    /// same way.
    #[cfg(unix)]
    fn stubbed_user_tree(base: &Path, body: &str) -> PathBuf {
        let user = base.join("python");
        let interpreter = interpreter_in_tree(&user);
        write_stub_interpreter(interpreter.parent().unwrap(), body);
        std::fs::write(user.join("sentinel.txt"), "kept").unwrap();
        user
    }

    #[cfg(unix)]
    #[test]
    fn startup_defers_when_the_probe_fails_on_a_usable_interpreter() {
        let base = unique_temp_dir("refresh-defer-startup");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = stubbed_user_tree(&base, PROBE_FAILS_BUT_USABLE);

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup)
            .expect("a deferred refresh is not a failed one");

        assert!(
            user.join("sentinel.txt").is_file(),
            "the tree must survive: wiping it would discard the pinned version \
             the probe just failed to read"
        );
        assert!(
            !user.join(PYTHON_VERSION_MARKER).exists(),
            "a defer must not leave a marker claiming the tree is current"
        );
        assert_eq!(
            read_counter(&user.join(PYTHON_REFRESH_DEFER_MARKER)),
            1,
            "the defer is counted, which is what makes it bounded"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn startup_stops_deferring_at_the_bound_and_wipes() {
        let base = unique_temp_dir("refresh-defer-bound-e2e");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = stubbed_user_tree(&base, PROBE_FAILS_BUT_USABLE);
        bump_counter(&user.join(PYTHON_REFRESH_DEFER_MARKER), MAX_REFRESH_DEFERS);

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();

        assert!(
            !user.join("sentinel.txt").exists(),
            "package metadata that is persistently unreadable must not gate the \
             self-heal behind itself forever"
        );
        assert_marker_current(&user);
        assert_eq!(
            read_counter(&user.join(PYTHON_REFRESH_DEFER_MARKER)),
            0,
            "the wipe takes the counter with the tree, so defers are consecutive"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn startup_wipes_when_the_interpreter_itself_is_unusable() {
        let base = unique_temp_dir("refresh-unusable");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = stubbed_user_tree(&base, "exit 1");
        // Ride out a transient ETXTBSY before driving the refresh: a concurrent
        // fork in another test thread can hold the just-written stub open for
        // writing, and an exec that fails for that reason makes the usability
        // check *unanswerable*, which defers — the opposite of what this test
        // asserts. Only the probe is retried; the refresh has side effects, so
        // it gets exactly one run. (Linux enforces ETXTBSY; macOS does not,
        // which is why only Linux CI flaked.)
        let interpreter = interpreter_in_tree(&user);
        for _ in 0..20 {
            if interpreter_is_usable(&interpreter).is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();

        assert!(
            !user.join("sentinel.txt").exists(),
            "an interpreter that cannot run has no pinned version worth \
             protecting; deferring would leave a corrupt tree with no repair path"
        );
        assert_marker_current(&user);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn startup_stops_deferring_when_the_counter_cannot_be_written() {
        // The other "stop deferring" path: not a reached bound, but a counter
        // that can never advance to one. Left deferring, an unwritable marker
        // reintroduces the unbounded loop the bound exists to stop.
        //
        // Reached by pre-creating the marker as a *directory*: `read_counter`
        // reads 0 (so the bound is nowhere near), while `atomic_write`'s rename
        // cannot land a file on it, so `bump_counter` returns false. This makes
        // only the counter path fail — chmod'ing the tree read-only would also
        // block the `remove_dir_all` the wipe then performs.
        let base = unique_temp_dir("refresh-defer-unwritable");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = stubbed_user_tree(&base, PROBE_FAILS_BUT_USABLE);
        std::fs::create_dir_all(user.join(PYTHON_REFRESH_DEFER_MARKER)).unwrap();
        assert_eq!(
            read_counter(&user.join(PYTHON_REFRESH_DEFER_MARKER)),
            0,
            "the bound must be out of reach, so only the failed write can stop the defer"
        );

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Startup).unwrap();

        assert!(
            !user.join("sentinel.txt").exists(),
            "a counter that can never advance must stop the defer now, not \
             defer forever waiting for a bound it cannot reach"
        );
        assert_marker_current(&user);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn repair_never_defers_even_when_the_probe_fails() {
        let base = unique_temp_dir("refresh-defer-repair");
        let _ = std::fs::remove_dir_all(&base);
        let bundle = fake_bundle(&base);
        let user = stubbed_user_tree(&base, PROBE_FAILS_BUT_USABLE);

        refresh_python_tree(&user, || Ok(bundle.clone()), RefreshReason::Repair).unwrap();

        assert!(
            !user.join("sentinel.txt").exists(),
            "a repair is called on a tree already proven broken; keeping it one \
             more launch answers a question nobody asked"
        );
        assert_marker_current(&user);

        let _ = std::fs::remove_dir_all(&base);
    }
}
