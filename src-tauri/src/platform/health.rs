//! Is the bundled Python tree healthy, and may we repair it?
//!
//! The probe runs a real `esphome config` against a minimal config, because
//! the failure class that motivated it (#330, an orphaned component package)
//! is invisible to every metadata check and only surfaces when ESPHome's
//! loader actually walks the tree. The repair counter bounds how often a
//! failed probe may trigger the reset, so a breakage the reset cannot fix
//! never becomes a wipe-reinstall loop.

use super::process::{run_python_capture_bounded, tail_for_log};
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Read a persisted attempt counter, returning 0 when the marker is missing or
/// unparseable (treat a damaged counter as a fresh start rather than blocking
/// the self-heal it bounds).
pub(super) fn read_counter(marker_path: &Path) -> u32 {
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
pub(super) fn bump_counter(marker_path: &Path, count: u32) -> bool {
    match crate::util::atomic_write(marker_path, count.to_string()) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Could not persist counter to {marker_path:?}: {e:#}");
            false
        }
    }
}

/// Hard upper bound on the health probe. Measured at ~0.2s against a real
/// bundled tree, so this is not a budget — it is the line between "slow" and
/// "never", on a path the user is waiting behind. Without it a wedged
/// interpreter means the backend never starts and nothing says why.
pub(super) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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
/// a repair. Lives at `<python parent dir>/.repair-count`
/// (see [`super::get_python_parent_dir`]).
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
/// Takes the tree's parent dir rather than the tree so the count survives a
/// repair that replaces the tree wholesale; see [`REPAIR_COUNT_MARKER`].
/// Nothing resets the budget implicitly — [`clear_repair_count`] does it, once
/// a probe actually passes.
pub fn may_repair_tree(python_parent_dir: &Path) -> bool {
    let marker = python_parent_dir.join(REPAIR_COUNT_MARKER);
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
pub fn repair_budget_left(python_parent_dir: &Path) -> bool {
    read_counter(&python_parent_dir.join(REPAIR_COUNT_MARKER)) < MAX_REPAIRS
}

/// Forget any recorded repair attempts, once the tree is proven healthy.
///
/// A missing marker is the normal case and says nothing. Any other failure does:
/// it pins the counter at [`MAX_REPAIRS`] forever, so a later and
/// perfectly fixable breakage is never repaired and the log only ever claims the
/// budget is spent. That is worth a line, for the same reason [`bump_counter`]
/// reports its own write failures rather than swallowing them.
pub fn clear_repair_count(python_parent_dir: &Path) {
    let marker = python_parent_dir.join(REPAIR_COUNT_MARKER);
    match std::fs::remove_file(&marker) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(
            "Could not clear the repair counter at {marker:?} ({e}); a future repair may be \
             refused as budget-spent"
        ),
    }
}

/// Resolve the root of a managed Python tree from its interpreter path.
///
/// `<root>/python.exe` on Windows, `<root>/bin/python3` elsewhere. Deriving the
/// root from the interpreter rather than rebuilding it from the data dir keeps
/// this correct for whichever tree [`super::get_python_path`] actually selected.
pub(super) fn python_tree_root(python_bin: &Path) -> Option<&Path> {
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

/// The interpreter path inside a Python tree laid out the way the real bundle
/// is on this platform: [`python_tree_root`]'s inverse. One spelling of the
/// shipped layout, shared between the path resolvers and the tests so a second
/// copy cannot drift from it.
pub(super) fn interpreter_in_tree(root: &Path) -> PathBuf {
    if cfg!(target_os = "windows") {
        root.join("python.exe")
    } else {
        root.join("bin").join("python3")
    }
}

/// Whether `python_bin` is a Python tree this app manages, as opposed to the
/// bare `python3`/`python` [`super::get_python_path`] falls back to in development
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;

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
}
