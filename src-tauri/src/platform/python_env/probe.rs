//! What a child interpreter can be asked: whether it runs at all, which
//! version of a package it reports, and the maintenance-script runners that
//! prune duplicate `.dist-info` metadata.
//!
//! Every function here answers a question by spawning the managed Python and
//! reading what comes back. That is a different job from [`super`], which
//! decides when the tree is copied or wiped — it consumes these answers, it
//! does not produce them.

use crate::platform::health::PROBE_TIMEOUT;
use crate::platform::process::{run_python_capture, run_python_capture_bounded, tail_for_log};
use anyhow::{Context, Result};
use std::path::Path;
use tracing::{info, warn};

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
/// answer that question. [`crate::platform::esphome_config_probe`] asks a bigger one —
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

/// Hard bound on one dist-info dedupe run. Local filesystem work only —
/// enumerate site-packages, remove a few directories of metadata text — so a
/// minute is generous even under a slow disk or an antivirus scan. Both
/// callers get the bound. It is load-bearing for the lazy heal, which holds
/// the `UpdateGuard` across this child; for the post-copy self-clean it caps
/// how long a wedged child can stall the launch and repair paths, which
/// block on the refresh.
const DIST_INFO_DEDUPE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Maintenance helper run with the bundled interpreter as `python -I -c <src>
/// <mode>`. `detect` prints the highest installed device-builder version (empty
/// if undeterminable); `dedupe` / `dedupe-all` remove orphaned duplicate
/// `.dist-info` dirs and print how many they removed. Embedded so it ships with
/// the binary and stays in sync with its pytest suite
/// (`tests/test_device_builder_maintenance.py`). Private: the argv-mode
/// contract is owned entirely by the two runners below.
const DEVICE_BUILDER_MAINT_PY: &str =
    include_str!("../../../scripts/device_builder_maintenance.py");

/// Which distributions [`dedupe_dist_info`] may prune.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DistInfoDedupeScope {
    /// Only `esphome-device-builder` and its frontend — the lazy #190 heal the
    /// device-builder update check runs when it cannot determine a version.
    /// Deliberately narrow even though the guards are scope-independent: this
    /// heal runs on a live user tree mid-session, so it prunes no more than
    /// the packages whose metadata it needs; the post-copy self-clean owns
    /// the whole-tree scope.
    DeviceBuilder,
    /// Every installed distribution — the self-clean each bundle copy runs so a
    /// dirty source tree still yields unambiguous metadata (#389).
    All,
}

/// Remove orphaned duplicate `.dist-info` directories, keeping the highest
/// version's metadata per package (the code on disk is whatever pip installed
/// last, i.e. the newest).
///
/// Duplicates make `importlib.metadata` answer with an arbitrary one of the
/// piled-up versions, which poisons everything that trusts it: the
/// device-builder update check loops on "version None" (#190), and the
/// pinned-version snapshot/restore around a tree refresh compares against a
/// stale version (#389). The prune itself lives in
/// [`DEVICE_BUILDER_MAINT_PY`], which never deletes an entry pip can still
/// manage but cannot be ranked; a RECORD-less entry is removed, because pip
/// aborts every upgrade with `uninstall-no-record-file` while one exists and
/// the bundled tree itself can carry one (#389). See the pytest suite for the
/// guard behavior.
///
/// `Err` covers both a failed spawn and a non-zero exit; callers on paths that
/// must not fail (the copy in [`super::refresh_python_tree`], the best-effort heal in
/// the update check) log and continue.
pub(crate) fn dedupe_dist_info(python_bin: &Path, scope: DistInfoDedupeScope) -> Result<()> {
    let mode = match scope {
        DistInfoDedupeScope::DeviceBuilder => "dedupe",
        DistInfoDedupeScope::All => "dedupe-all",
    };
    // `-I` (isolated) keeps user site-packages, PYTHONPATH and sitecustomize
    // off sys.path, so the prune sees only the interpreter's own
    // site-packages. Which interpreter is the caller's choice: the post-copy
    // self-clean passes the managed tree's own binary, while the lazy heal
    // resolves through `get_python_path` — the managed tree in production,
    // but the bare system fallback in a bundle-less dev build, where the
    // TARGETS scope is what limits the prune's reach.
    //
    // Bounded: the lazy heal holds the UpdateGuard across this child, so a
    // wedged prune (an rmtree stuck on a handle-held path) would otherwise pin
    // `update_in_flight` for the session and silently no-op every later
    // update/switch arm. The work is local filesystem only — enumerate
    // site-packages, remove a few directories — so the bound is generous.
    let output = run_python_capture_bounded(
        python_bin,
        ["-I", "-c", DEVICE_BUILDER_MAINT_PY, mode],
        DIST_INFO_DEDUPE_TIMEOUT,
    )
    .context("Failed to run dist-info dedup")?;
    if !output.status.success() {
        anyhow::bail!(
            "dist-info dedup ({mode}) exited non-zero: {}",
            tail_for_log(&String::from_utf8_lossy(&output.stderr))
        );
    }
    // The helper logs dist-info it couldn't read or remove to stderr; surface it
    // (bounded) so a partial prune isn't silently lost.
    let stderr = tail_for_log(&String::from_utf8_lossy(&output.stderr));
    if !stderr.is_empty() {
        warn!("dist-info dedup ({mode}): {stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let removed = stdout.trim();
    if !removed.is_empty() && removed != "0" {
        info!("Removed {removed} stale dist-info dir(s) ({mode})");
    }
    Ok(())
}

/// Get the installed `esphome-device-builder` package version using the
/// maintenance helper's `detect` mode, which enumerates every matching
/// distribution and takes the highest version — robust to the duplicate
/// dist-info pileup that makes a plain `importlib.metadata.version(...)`
/// return None or an older version (#190).
///
/// - `Ok(Some(v))` — package is installed, returns the version string.
/// - `Ok(None)` — `detect` ran successfully (exit 0) but printed no version:
///   the package is not installed, or duplicate dist-info dirs left it
///   undeterminable (#190).
/// - `Err(_)` — detection itself failed: the spawn failed or the helper exited
///   non-zero (a broken interpreter / import error). Callers should surface
///   this rather than treat it as "not installed".
pub(crate) fn detect_device_builder_version(python_bin: &Path) -> Result<Option<String>> {
    // `-I` (isolated) keeps user site-packages, PYTHONPATH and sitecustomize off
    // sys.path so detection only ever sees the managed bundled install.
    let output = run_python_capture(python_bin, ["-I", "-c", DEVICE_BUILDER_MAINT_PY, "detect"])
        .context("Failed to run python")?;
    if !output.status.success() {
        // `detect` exits 0 even when the package is absent (it prints nothing),
        // so a non-zero exit is a real execution failure.
        anyhow::bail!(
            "device-builder version detection failed: {}",
            tail_for_log(&String::from_utf8_lossy(&output.stderr))
        );
    }
    // `detect` logs skipped/unreadable distributions to stderr; surface it so
    // the reason a version came back undeterminable isn't lost.
    let stderr = tail_for_log(&String::from_utf8_lossy(&output.stderr));
    if !stderr.is_empty() {
        warn!("device-builder version detection: {stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout.trim();
    // Empty means "not determinable" (no install or an unresolvable pileup);
    // the "None" guard is belt-and-suspenders. Either way the caller must not
    // be offered an endless update (#190).
    if version.is_empty() || version == "None" {
        return Ok(None);
    }
    Ok(Some(version.to_string()))
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
///   [`super::snapshot_preserved_versions`].
pub(crate) fn read_package_version(python_bin: &Path, package: &str) -> Result<Option<String>> {
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

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::super::{log_last_arg_stub, write_stub_interpreter};
    use super::*;
    #[cfg(unix)]
    use crate::util::unique_temp_dir;

    #[test]
    fn maint_script_pins_the_argv_mode_contract() {
        // Behavior, including main()'s dispatch and exit codes, is covered by
        // tests/test_device_builder_maintenance.py; here we only pin the mode
        // *names* the two runners above pass on argv, so a rename can't pass
        // silently. Quoted, so the bare "dedupe" can't be satisfied by the
        // "dedupe-all" literal, and no statement shape is pinned.
        assert!(DEVICE_BUILDER_MAINT_PY.contains("\"detect\""));
        assert!(DEVICE_BUILDER_MAINT_PY.contains("\"dedupe\""));
        assert!(DEVICE_BUILDER_MAINT_PY.contains("\"dedupe-all\""));
        assert!(DEVICE_BUILDER_MAINT_PY.contains("esphome-device-builder-frontend"));
    }

    #[cfg(unix)]
    #[test]
    fn dedupe_scope_selects_the_script_mode() {
        let base = unique_temp_dir("dedupe-scope");
        let log = base.join("calls.log");
        let bin = write_stub_interpreter(&base, &log_last_arg_stub(&log));

        dedupe_dist_info(&bin, DistInfoDedupeScope::DeviceBuilder).unwrap();
        dedupe_dist_info(&bin, DistInfoDedupeScope::All).unwrap();

        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls.lines().collect::<Vec<_>>(),
            ["dedupe", "dedupe-all"],
            "each scope must map to its maintenance-script mode"
        );

        let _ = std::fs::remove_dir_all(&base);
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
}
