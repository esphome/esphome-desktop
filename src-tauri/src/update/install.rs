//! Package install, health-probe, and broken-tree repair helpers.
//!
//! Everything in the update module that shells out to pip/Python or notifies
//! the user that a managed Python tree could not be repaired.

use anyhow::{Context, Result};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use tracing::{info, warn};

use crate::control::ops::UpdateGuard;
use crate::i18n::{t, t_with};
use crate::platform;
use crate::settings::Backend;

/// Get the installed `esphome-device-builder` package version. Result
/// semantics (including why `Err` must not be read as "not installed") are
/// [`platform::detect_device_builder_version`]'s.
pub fn get_installed_device_builder_version(app_handle: &AppHandle) -> Result<Option<String>> {
    let python_path = platform::get_python_path(app_handle)?;
    platform::detect_device_builder_version(&python_path)
}

/// Remove orphaned duplicate `.dist-info` directories for the device-builder
/// package and its frontend, keeping the highest version's metadata.
///
/// The `--ignore-installed` install fallback skipped the uninstall and left the
/// previous version's `.dist-info` behind; once several pile up,
/// `importlib.metadata` can no longer resolve a single version and the updater
/// loops forever offering "version None" (#190). This heals that state. Error
/// semantics are [`platform::dedupe_dist_info`]'s; the one caller treats an
/// `Err` as best-effort, logging rather than blocking the update check.
///
/// The `--ignore-installed` fallback that caused most of this damage is gone
/// (see [`install_with_record_recovery`]), but the damage still arises: the
/// installer overlays the install dir without deleting the previous release's
/// files, so the bundled tree itself can carry duplicate dist-info dirs (#389).
/// Each bundle copy now prunes itself
/// ([`platform::dedupe_dist_info`] runs in `dedupe-all` scope after the copy),
/// so this lazy device-builder-scoped heal mostly covers trees that predate
/// that self-clean; duplicate metadata does not fail an install or the
/// `esphome config` health probe, so nothing else would recover them.
///
/// The prune also removes RECORD-less dist-info dirs. Those do fail an
/// install — pip aborts with `uninstall-no-record-file` — and when the
/// overlay plants one in the bundled tree, the repair re-copy restores it, so
/// without this removal the [`install_with_record_recovery`] retry would hit
/// the identical abort forever ("Cannot uninstall
/// esphome-device-builder-frontend None").
pub fn dedupe_device_builder_dist_info(app_handle: &AppHandle) -> Result<()> {
    let python_path = platform::get_python_path(app_handle)?;
    platform::dedupe_dist_info(&python_path, platform::DistInfoDedupeScope::DeviceBuilder)
}

/// The `UpdateGuard` flag, when the app state is managed (it is not in unit
/// tests).
fn update_guard_flag(app_handle: &AppHandle) -> Option<Arc<AtomicBool>> {
    use tauri::Manager;
    app_handle
        .try_state::<Arc<crate::AppState>>()
        .map(|state| state.update_in_flight.clone())
}

/// What the caller of [`detect_device_builder_version_with_heal_async`] knows
/// about the `UpdateGuard`, which decides whether the destructive dist-info
/// heal may run.
///
/// A global flag read alone cannot make this decision: it cannot tell
/// "someone else's sequence is running" from "my own caller holds the guard",
/// and the two mean opposite things for the heal (a guard the caller holds is
/// proof that no pip install can be racing).
#[derive(Clone, Copy)]
pub(super) enum HealPolicy<'g> {
    /// The caller holds the `UpdateGuard` (the tray's Check for Updates arm),
    /// so its sequence is the only one running and the steps are sequential:
    /// no pip install can be concurrently in flight when the heal runs. This
    /// is also the main user-facing recovery for the #190 pileup, so the heal
    /// must not stand down here. The reference is the proof: a guardless
    /// caller cannot claim this variant without acquiring a guard first. It
    /// is never read — possession at construction is its whole job.
    GuardHeld(#[allow(dead_code)] &'g UpdateGuard),
    /// The caller runs without the guard (the daily background check). The
    /// heal takes the guard itself for its duration, or stands down if one is
    /// in flight: pip writes `RECORD` last, so a package mid-install passes
    /// through exactly the RECORD-less shape the prune condemns.
    WhenIdle,
}

/// Secure permission to run the destructive heal. `None` means denied.
///
/// On the guardless path this *takes* the `UpdateGuard` (returned inside the
/// permit so it is held, RAII, for the heal's duration) rather than sampling
/// the flag: a read alone is check-then-act, and a sequence acquiring between
/// the read and the Python spawn would race the rmtree into a live pip
/// install. `try_acquire` keeps it non-blocking. A caller that already holds
/// the guard must not re-acquire (it would deny against itself), and with no
/// managed state (unit tests) there is no guard to take.
fn acquire_heal_permit(
    caller_holds_guard: bool,
    guard_flag: Option<Arc<AtomicBool>>,
) -> Option<Option<UpdateGuard>> {
    if caller_holds_guard {
        return Some(None);
    }
    match guard_flag {
        None => Some(None),
        Some(flag) => UpdateGuard::try_acquire(flag).map(Some),
    }
}

/// Orchestrate detect → permit → heal → re-detect.
///
/// A `None` detection is the exact symptom of the pileup (#190), so prune the
/// duplicates and re-query before giving up. A genuinely-absent package stays
/// `None` (dedup finds nothing to remove), at the cost of one extra Python
/// spawn only on the already-unusual not-determinable path. A denied permit
/// skips the heal: an undeterminable version then is far more likely a
/// concurrent install's transient state than the #190 pileup, and a real
/// pileup is still healed by the next check once the guard frees. A failed
/// heal still re-queries — best-effort, but not invisible.
///
/// The steps are injected, exactly like [`install_with_record_recovery`]'s,
/// so this policy stays testable without an `AppHandle` or a live
/// interpreter.
fn heal_and_redetect<D, H, P>(detect: D, heal: H, permit: P) -> Result<Option<String>>
where
    D: Fn() -> Result<Option<String>>,
    H: FnOnce() -> Result<()>,
    P: FnOnce() -> Option<Option<UpdateGuard>>,
{
    let installed = detect()?;
    if installed.is_some() {
        return Ok(installed);
    }
    let Some(_permit) = permit() else {
        info!("skipping the dist-info heal: another update sequence is in flight");
        return Ok(installed);
    };
    if let Err(e) = heal() {
        // The heal is best-effort, but a failed attempt shouldn't be invisible:
        // the re-query below will just return the same undeterminable result.
        warn!("device-builder dist-info heal failed: {e}");
    }
    detect()
}

/// Detect the installed device-builder version, healing a duplicate dist-info
/// pileup once if the first lookup cannot determine a version.
///
/// The policy lives in [`heal_and_redetect`]; whether the heal may run is the
/// caller's knowledge, not this function's (see [`HealPolicy`] and
/// [`acquire_heal_permit`]).
fn detect_device_builder_version_with_heal(
    app_handle: &AppHandle,
    caller_holds_guard: bool,
) -> Result<Option<String>> {
    heal_and_redetect(
        || get_installed_device_builder_version(app_handle),
        || dedupe_device_builder_dist_info(app_handle),
        || acquire_heal_permit(caller_holds_guard, update_guard_flag(app_handle)),
    )
}

/// Async wrapper around the blocking [`detect_device_builder_version_with_heal`].
///
/// The underlying detection shells out to pip/Python (and may spawn a second
/// process for the dist-info heal), so calling it directly from an async task
/// would block a tokio worker thread. The update-check flows dispatch it via
/// `spawn_blocking` instead.
pub(super) async fn detect_device_builder_version_with_heal_async(
    app_handle: &AppHandle,
    policy: HealPolicy<'_>,
) -> Result<Option<String>> {
    // The guard reference proves possession at the call site but cannot cross
    // spawn_blocking's 'static boundary; reduce it to the fact the blocking
    // side acts on.
    let caller_holds_guard = matches!(policy, HealPolicy::GuardHeld(_));
    let app = app_handle.clone();
    tokio::task::spawn_blocking(move || {
        detect_device_builder_version_with_heal(&app, caller_holds_guard)
    })
    .await
    .context("device-builder version detection task panicked or was cancelled")?
}

/// Build the `pip install` argument list (appended after the `-m pip install`
/// prefix supplied by [`crate::platform::pip_command`]) for installing/upgrading
/// `esphome-device-builder`.
///
/// A plain `pip install --upgrade`, which uninstalls the existing copy cleanly
/// first. There is deliberately no `--ignore-installed` variant: see
/// [`install_with_record_recovery`].
///
/// `version` pins the package to an exact release (`esphome-device-builder==X`).
/// A plain `--upgrade` never *downgrades*, so switching the device builder from
/// a newer beta to an older stable would otherwise be a silent no-op (#200).
/// Passing the resolved stable version forces pip to install exactly that
/// release, downgrading off the newer beta. Pass `None` to keep the package
/// unpinned (the beta channel, which only ever moves forward).
fn device_builder_install_args(backend: Backend, version: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec!["--upgrade".to_string()];
    if backend == Backend::BuilderBeta {
        args.push("--pre".to_string());
    }
    match version {
        Some(v) => args.push(format!("esphome-device-builder=={v}")),
        None => args.push("esphome-device-builder".to_string()),
    }
    args
}

/// Run `pip install` for `esphome-device-builder`.
pub(super) async fn run_device_builder_install(
    python_path: &std::path::Path,
    backend: Backend,
    version: Option<&str>,
) -> Result<std::process::Output> {
    let args = device_builder_install_args(backend, version);
    let mut cmd = platform::pip_command(python_path);
    cmd.args(&args);
    platform::run_pip(cmd)
        .await
        .context("Failed to run pip install")
}

/// URL of the ESPHome dev-branch source archive installed on the Dev channel.
const ESPHOME_DEV_ZIP_URL: &str = "https://github.com/esphome/esphome/archive/dev.zip";

/// Run `pip install` for the ESPHome dev GitHub zip.
///
/// A plain `--force-reinstall`, which uninstalls the existing copy of each
/// affected package first. There is deliberately no `--ignore-installed`
/// variant: see [`install_with_record_recovery`].
pub(super) async fn run_dev_install(python_path: &std::path::Path) -> Result<std::process::Output> {
    let mut cmd = platform::pip_command(python_path);
    cmd.args(["--force-reinstall", ESPHOME_DEV_ZIP_URL]);
    platform::run_pip(cmd)
        .await
        .context("Failed to run pip install")
}

/// Run `pip install` for a pinned stable/beta ESPHome release (`esphome==X`).
///
/// A plain pinned install, which uninstalls the differing installed copy first.
/// The pin is what lets it downgrade off a newer installed copy. There is
/// deliberately no `--ignore-installed` variant: see
/// [`install_with_record_recovery`].
pub(super) async fn run_esphome_install(
    python_path: &std::path::Path,
    version: &str,
) -> Result<std::process::Output> {
    let mut cmd = platform::pip_command(python_path);
    cmd.arg(format!("esphome=={version}"));
    platform::run_pip(cmd)
        .await
        .context("Failed to run pip install")
}

/// What the user can actually do about a tree we could not repair.
///
/// Keyed on whether another attempt is genuinely coming. Once the budget is
/// spent nothing retries until a probe passes, so promising a retry there
/// would be a fresh falsehood in place of the one this message already
/// dropped — and it would repeat on every launch, forever.
///
/// With attempts left, "reopen and we will try again" is true.
///
/// With none left, the advice has to be something that works, and reinstalling
/// the app is not it: the tree lives in app data on every platform (#335), and
/// an app reinstall never touches it, since `ensure_user_python` only
/// re-copies when the version marker changes. What does work is removing the
/// tree — the next launch finds no interpreter and re-copies the bundle — so
/// name that, and name the path.
pub(super) fn repair_hint(python_parent_dir: &std::path::Path, retryable: bool) -> String {
    if retryable {
        return t("update.repair_hint_retry");
    }
    t_with(
        "update.repair_hint_delete_tree",
        &[(
            "path",
            &python_parent_dir
                .join(platform::PYTHON_TREE_DIRNAME)
                .display()
                .to_string(),
        )],
    )
}

/// Ask whether the interpreter itself runs, off the async executor.
///
/// `Err` is a failed check, not a failed interpreter, and the two must not be
/// collapsed: a panicking check would otherwise read as an affirmative "the
/// interpreter is fine" that nothing established, and the caller would skip a
/// repair on the strength of it. Hence `Result<bool, JoinError>` rather than the
/// flattening [`probe_esphome`] does — there, nothing acts on the distinction;
/// here, the caller has a third arm for exactly it.
pub(super) async fn interpreter_usable(python_path: &std::path::Path) -> Result<bool> {
    let python = python_path.to_path_buf();
    tokio::task::spawn_blocking(move || platform::interpreter_is_usable(&python))
        .await
        .context("the interpreter usability check panicked or was cancelled")?
        .context("could not run the interpreter usability check")
}

/// Run the ESPHome health probe off the async executor.
///
/// The probe spawns an interpreter and waits on it, so it cannot run on a tokio
/// worker. Flattening the `JoinError` into the probe's own error here means
/// callers get one three-armed answer — healthy, broken, or unknown — instead of
/// each re-deciding what a panicked task means. The distinction survives in the
/// error chain, which is where it belongs: nothing acts on it differently.
///
/// Deliberately the opposite call to [`interpreter_usable`]'s, for a reason that
/// is easy to lose: a probe that could not run and a probe that ran and found
/// damage both mean "we have no clean answer", and the caller treats them alike.
/// Whether the *interpreter* runs is the question the caller then branches on, so
/// that one must not lose its failure mode.
pub(super) async fn probe_esphome(python_path: &std::path::Path) -> Result<Option<String>> {
    let python = python_path.to_path_buf();
    tokio::task::spawn_blocking(move || platform::esphome_config_probe(&python))
        .await
        .context("ESPHome health probe task panicked or was cancelled")?
}

/// Tell the user the tree is still broken after a repair, or that we could
/// not confirm it is not.
pub(super) fn notify_repair_incomplete(
    app_handle: &AppHandle,
    python_parent_dir: &std::path::Path,
) {
    let retryable = platform::repair_budget_left(python_parent_dir);
    notify_repair_needed(
        app_handle,
        t_with(
            "update.repair_incomplete",
            &[("hint", &repair_hint(python_parent_dir, retryable))],
        ),
    );
}

/// Tell the user their ESPHome install is broken and we could not fix it.
///
/// A notification rather than a modal, matching `notify_if_git_missing`: like a
/// missing git, this is a persistent condition found during an unprompted
/// startup check, not the result of anything the user just asked for. The modal
/// `dialog::notice` calls in this module all answer a user-initiated update, so
/// a dialog is expected there and would be an ambush here. It re-fires on each
/// launch while the tree stays broken, matching the git-missing cadence — every
/// build fails until it is dealt with, so a one-shot warning that scrolls out of
/// the log is not enough.
pub(super) fn notify_repair_needed(app_handle: &AppHandle, body: String) {
    if let Err(e) = app_handle
        .notification()
        .builder()
        .title(t("update.repair_failed_title"))
        .body(body)
        .show()
    {
        warn!("Failed to show the ESPHome repair notification: {e}");
    }
}

/// Detect pip's missing-RECORD abort: the uninstall step cannot run because a
/// package has no `dist-info/RECORD` listing its files (#155/#183).
///
/// This is the signal that the tree is corrupt rather than the install being
/// wrong, so it is what selects the repair path in
/// [`install_with_record_recovery`].
fn is_missing_record_error(stderr: &str) -> bool {
    stderr.contains("uninstall-no-record-file") || stderr.contains("no RECORD file was found")
}

/// Run a pip install, repairing the tree if pip cannot uninstall the old copy
/// (#155/#183/#330).
///
/// `run` performs a normal install. If it aborts because a package has no
/// `dist-info/RECORD` (`is_missing_record_error`), the tree is corrupt rather
/// than the install being wrong, so `repair` restores it and `run` is tried once
/// more against the clean tree. Any other failure bails immediately, reported
/// through [`platform::pip_output_report`] so a resolution failure carries its
/// cause from both of pip's streams.
///
/// The retry can only succeed if the repair actually clears the offending
/// dist-info, and a re-copy alone does not when the bundled tree itself
/// carries it (the installer overlay, #389): the post-copy dedupe's removal
/// of RECORD-less dist-info dirs is what breaks that loop (see
/// [`dedupe_device_builder_dist_info`]).
///
/// The repair puts back the version the user had and `run` then installs the
/// target over it, so this path can pip-install twice. That is deliberate, not
/// an oversight: the repair cannot know which package its caller is about to
/// install, and one that skipped the restore would silently downgrade whatever
/// the caller was *not* installing — `install_device_builder` would leave esphome
/// on the bundled version. Teaching the repair about the caller's target would
/// couple the tree refresh to the update flow to save one install on a path that
/// only runs when the tree is already corrupt.
///
/// The previous recovery here retried with `--ignore-installed`, which is pip's
/// documented way past a missing RECORD but skips the uninstall entirely. That
/// silently orphans every file the old version had and the new one does not: a
/// stale `esphome/components/rp2040/` broke every compile on 2026.7 (#330), and
/// stale `.dist-info` dirs broke version detection (#190) — a bug we then
/// shipped a maintenance script to clean up after. Repairing the tree properly
/// removes the whole class rather than treating each symptom, and costs a
/// reinstall on a path that only runs when the tree is already broken.
///
/// `repair` is injected rather than called directly so the policy stays
/// testable without a live interpreter or PyPI.
pub(super) async fn install_with_record_recovery<F, Fut, R, RFut>(
    run: F,
    repair: R,
    success_msg: &str,
    fail_prefix: &str,
) -> Result<()>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<std::process::Output>>,
    R: FnOnce() -> RFut,
    RFut: std::future::Future<Output = Result<()>>,
{
    let output = run().await?;
    if output.status.success() {
        info!("{success_msg}");
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    if !is_missing_record_error(&stderr) {
        anyhow::bail!("{fail_prefix}: {}", platform::pip_output_report(&output));
    }

    info!("{fail_prefix}: missing RECORD file; repairing the Python tree");
    repair().await.context("ESPHome repair failed")?;

    let retry = run().await?;
    if retry.status.success() {
        info!("{success_msg} (after repairing the Python tree)");
        Ok(())
    } else {
        anyhow::bail!("{fail_prefix}: {}", platform::pip_output_report(&retry));
    }
}

/// Installed ESPHome version, distinguishing "not installed" from a real
/// detection failure: `Ok(Some(v))` when installed, `Ok(None)` when the
/// `esphome version` command runs but exits non-zero (ESPHome absent), and
/// `Err` only when the check itself can't run (e.g. Python missing). Every
/// caller handles `Ok(None)` explicitly, mirroring the device-builder
/// `get_installed_device_builder_version` shape so "not installed" and
/// "detection failed" never collapse into one state.
pub fn installed_esphome_version(app_handle: &AppHandle) -> Result<Option<String>> {
    let python_path = platform::get_python_path(app_handle)?;

    let Some(version) =
        platform::run_python_capture_stdout(&python_path, ["-m", "esphome", "version"])
            .context("Failed to run esphome version")?
    else {
        return Ok(None);
    };
    // Extract just the version number
    let version = version
        .strip_prefix("Version: ")
        .unwrap_or(&version)
        .to_string();
    Ok(Some(version))
}

/// Async wrapper around the blocking [`installed_esphome_version`] detection.
///
/// `installed_esphome_version` runs `python -m esphome version`, whose esphome
/// import can take several seconds. Calling it directly from an async task
/// blocks a tokio worker thread for that whole time, so the update-check flows
/// dispatch it via `spawn_blocking` instead.
pub(super) async fn installed_esphome_version_async(
    app_handle: &AppHandle,
) -> Result<Option<String>> {
    let app = app_handle.clone();
    tokio::task::spawn_blocking(move || installed_esphome_version(&app))
        .await
        .context("esphome version detection task panicked or was cancelled")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// True if the owned-`String` arg list contains the given flag.
    fn has(args: &[String], flag: &str) -> bool {
        args.iter().any(|a| a == flag)
    }

    #[test]
    fn test_device_builder_install_args_never_ignore_installed() {
        // --ignore-installed skipped pip's uninstall and orphaned the old
        // version's files, which is what broke every compile in #330. No input
        // may reintroduce it; a broken tree is repaired by re-copying the
        // bundled tree instead.
        for backend in [Backend::BuilderStable, Backend::BuilderBeta] {
            for version in [None, Some("1.2.3")] {
                let args = device_builder_install_args(backend, version);
                assert!(
                    !has(&args, "--ignore-installed"),
                    "backend {backend:?} version {version:?}"
                );
                assert!(has(&args, "--upgrade"), "backend {backend:?}");
            }
        }
    }

    #[test]
    fn test_device_builder_install_args_default_unpinned() {
        for backend in [Backend::BuilderStable, Backend::BuilderBeta] {
            let args = device_builder_install_args(backend, None);
            assert!(has(&args, "--upgrade"), "backend {backend:?}");
            assert_eq!(
                args.last().map(String::as_str),
                Some("esphome-device-builder")
            );
        }
    }

    #[test]
    fn test_device_builder_install_args_pre_only_for_beta() {
        assert!(has(
            &device_builder_install_args(Backend::BuilderBeta, None),
            "--pre"
        ));
        assert!(!has(
            &device_builder_install_args(Backend::BuilderStable, None),
            "--pre"
        ));
    }

    #[test]
    fn test_device_builder_install_args_pins_version_for_downgrade() {
        // The #200 fix: passing an explicit version pins the package to that
        // exact release (`==X`). A plain `--upgrade` never downgrades, so the
        // pin is what forces pip off a newer installed beta onto the older
        // stable when switching channels.
        let args = device_builder_install_args(Backend::BuilderStable, Some("1.2.3"));
        assert!(has(&args, "--upgrade"));
        assert!(!has(&args, "--pre"));
        assert_eq!(
            args.last().map(String::as_str),
            Some("esphome-device-builder==1.2.3")
        );
    }

    #[test]
    fn the_heal_permit_takes_the_guard_rather_than_sampling_it() {
        use std::sync::atomic::Ordering;

        // A caller holding the UpdateGuard is proof no pip install can be
        // racing, and re-acquiring would deny against itself: permitted
        // without touching the flag.
        let flag = Arc::new(AtomicBool::new(false));
        assert!(matches!(acquire_heal_permit(true, None), Some(None)));
        assert!(matches!(
            acquire_heal_permit(true, Some(flag.clone())),
            Some(None)
        ));
        assert!(
            !flag.load(Ordering::Acquire),
            "GuardHeld must not acquire: its caller owns the flag"
        );

        // The guardless path with the flag free: the permit HOLDS the guard
        // for the heal's duration — sampling alone would be check-then-act,
        // letting a sequence start between the read and the rmtree — and
        // releases it on drop.
        let permit =
            acquire_heal_permit(false, Some(flag.clone())).expect("a free flag permits the heal");
        assert!(
            flag.load(Ordering::Acquire),
            "the permit must hold the flag, not sample it"
        );
        drop(permit);
        assert!(!flag.load(Ordering::Acquire), "released on drop");

        // The guardless path while a sequence is in flight: denied.
        let held = UpdateGuard::try_acquire(flag.clone()).expect("flag is free");
        assert!(acquire_heal_permit(false, Some(flag.clone())).is_none());
        drop(held);

        // No managed state (unit tests): no guard exists to take.
        assert!(matches!(acquire_heal_permit(false, None), Some(None)));
    }

    /// Drive [`heal_and_redetect`] against canned detections. `detections` is
    /// what each successive detect returns; `permit` grants or denies the
    /// heal. Returns the result plus (detects run, heals run).
    fn drive_heal(
        detections: Vec<Result<Option<String>>>,
        permit_granted: bool,
        heal: Result<()>,
    ) -> (Result<Option<String>>, (usize, usize)) {
        use std::cell::{Cell, RefCell};

        let queue = RefCell::new(detections.into_iter());
        let detects = Cell::new(0);
        let heals = Cell::new(0);
        let heal = RefCell::new(Some(heal));

        let result = heal_and_redetect(
            || {
                detects.set(detects.get() + 1);
                queue
                    .borrow_mut()
                    .next()
                    .expect("detect ran more times than the test planned for")
            },
            || {
                heals.set(heals.get() + 1);
                heal.borrow_mut().take().expect("healed twice")
            },
            || permit_granted.then_some(None),
        );
        (result, (detects.get(), heals.get()))
    }

    #[test]
    fn heal_and_redetect_returns_a_determinable_version_without_healing() {
        // A version the first lookup resolves needs no heal: the destructive
        // prune must not run on a healthy tree.
        let (result, counts) = drive_heal(vec![Ok(Some("1.8.2".into()))], true, Ok(()));
        assert_eq!(result.unwrap().as_deref(), Some("1.8.2"));
        assert_eq!(counts, (1, 0));
    }

    #[test]
    fn heal_and_redetect_heals_once_and_requeries() {
        // The #190 shape: undeterminable, then the prune clears the pileup
        // and the re-query resolves.
        let (result, counts) = drive_heal(vec![Ok(None), Ok(Some("1.8.2".into()))], true, Ok(()));
        assert_eq!(result.unwrap().as_deref(), Some("1.8.2"));
        assert_eq!(counts, (2, 1));
    }

    #[test]
    fn heal_and_redetect_stands_down_without_a_permit() {
        // A denied permit means an update sequence is in flight, and an
        // undeterminable version is then more likely pip mid-install than
        // the pileup: no heal, no second spawn, the None is reported as is.
        let (result, counts) = drive_heal(vec![Ok(None)], false, Ok(()));
        assert_eq!(result.unwrap(), None);
        assert_eq!(counts, (1, 0));
    }

    #[test]
    fn heal_and_redetect_requeries_even_after_a_failed_heal() {
        // Best-effort, but not silent, and never a reason to skip the
        // re-query: the second lookup reports whatever is true afterwards.
        let (result, counts) = drive_heal(
            vec![Ok(None), Ok(None)],
            true,
            Err(anyhow::anyhow!("interpreter is broken")),
        );
        assert_eq!(result.unwrap(), None);
        assert_eq!(counts, (2, 1));
    }

    #[test]
    fn heal_and_redetect_propagates_a_failed_detection_unhealed() {
        // Err means the check itself could not run (Python missing), which
        // implicates nothing about the tree: pruning on it would be
        // destructive action on zero evidence.
        let (result, counts) =
            drive_heal(vec![Err(anyhow::anyhow!("no interpreter"))], true, Ok(()));
        assert!(result.is_err());
        assert_eq!(counts, (1, 0));
    }

    #[test]
    fn test_is_missing_record_error() {
        assert!(is_missing_record_error("error: uninstall-no-record-file"));
        assert!(is_missing_record_error(
            "Cannot uninstall esphome-device-builder ...: no RECORD file was found"
        ));
        assert!(!is_missing_record_error("some other pip failure"));
    }

    #[test]
    fn repair_notification_strings_resolve() {
        // A missing key only warns and renders as the key itself, so an
        // unresolved one would ship `update.repair_failed` to the user as the
        // body of the notification telling them their install is broken.
        let python_parent_dir = Path::new("/data/io.esphome.builder");
        for retryable in [true, false] {
            for (body, key) in [
                (
                    t_with(
                        "update.repair_failed",
                        &[
                            ("error", "boom"),
                            ("hint", &repair_hint(python_parent_dir, retryable)),
                        ],
                    ),
                    "update.repair_failed",
                ),
                (
                    t_with(
                        "update.repair_incomplete",
                        &[("hint", &repair_hint(python_parent_dir, retryable))],
                    ),
                    "update.repair_incomplete",
                ),
            ] {
                assert_ne!(body, key, "{key} is missing from the translations");
                assert!(
                    !body.contains('{'),
                    "{key} has an unfilled placeholder: {body}"
                );
            }
        }
        assert_ne!(
            t("update.repair_failed_title"),
            "update.repair_failed_title"
        );
        assert!(
            t_with("update.repair_failed", &[("error", "boom"), ("hint", "")]).contains("boom")
        );
    }

    #[test]
    fn the_repair_hint_only_promises_a_retry_that_can_happen() {
        // The notice fires again on every launch while the tree stays broken, so
        // a hint that promises a retry after the budget is spent is a falsehood
        // repeated forever.
        let python_parent_dir = Path::new("/data/io.esphome.builder");

        let retryable = repair_hint(python_parent_dir, true);
        assert!(
            retryable.contains("Reopening"),
            "with attempts left, reopening really does retry: {retryable}"
        );

        let exhausted = repair_hint(python_parent_dir, false);
        assert!(
            !exhausted.contains("Reopening"),
            "with no attempts left nothing retries; promising one is a lie: {exhausted}"
        );

        // With no retry coming, the advice has to be something that works. The
        // tree sits in app data on every platform, untouched by an app
        // reinstall, so the hint must name the tree to remove instead.
        assert!(
            !exhausted.contains("reinstall"),
            "reinstalling does not touch the app-data tree: {exhausted}"
        );
        assert!(
            exhausted.contains(
                &python_parent_dir
                    .join(platform::PYTHON_TREE_DIRNAME)
                    .display()
                    .to_string()
            ),
            "name the folder the user has to remove: {exhausted}"
        );
    }

    #[test]
    fn test_is_missing_record_error_dev_zeroconf() {
        // The #183 dev-channel failure: a dependency (zeroconf) lacks a RECORD
        // file, which must also select the repair path.
        assert!(is_missing_record_error(
            "error: uninstall-no-record-file\n\n× Cannot uninstall zeroconf None\n╰─> The package's contents are unknown: no RECORD file was found for zeroconf."
        ));
    }

    /// Build a canned `Output` with the given success flag and stderr, so the
    /// recovery orchestration can be unit-tested without spawning pip.
    fn fake_output(success: bool, stderr: &str) -> std::process::Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            // Unix wait-status: 0 is success; exit code 1 encodes as 1 << 8.
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 << 8 })
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            std::process::ExitStatus::from_raw(if success { 0 } else { 1 })
        };
        std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    /// Drive `install_with_record_recovery` against canned pip outcomes.
    ///
    /// `attempts` is what each successive install returns; `repair` is what the
    /// repair returns if one is triggered. Returns the overall result plus
    /// (installs attempted, repairs attempted), which is the whole contract:
    /// how many times pip ran, and whether the tree was repaired between.
    async fn drive_recovery(
        attempts: Vec<std::process::Output>,
        repair: Result<()>,
    ) -> (Result<()>, (usize, usize)) {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        };

        let queue = Arc::new(Mutex::new(attempts.into_iter()));
        let installs = Arc::new(AtomicUsize::new(0));
        let repairs = Arc::new(AtomicUsize::new(0));
        let repair = Arc::new(Mutex::new(Some(repair)));

        let (run_installs, run_queue) = (installs.clone(), queue.clone());
        let (repair_count, repair_slot) = (repairs.clone(), repair.clone());

        let result = install_with_record_recovery(
            move || {
                let (installs, queue) = (run_installs.clone(), run_queue.clone());
                async move {
                    installs.fetch_add(1, Ordering::SeqCst);
                    Ok(queue
                        .lock()
                        .unwrap()
                        .next()
                        .expect("install ran more times than the test planned for"))
                }
            },
            move || async move {
                repair_count.fetch_add(1, Ordering::SeqCst);
                repair_slot.lock().unwrap().take().expect("repaired twice")
            },
            "ok",
            "failed",
        )
        .await;

        (
            result,
            (
                installs.load(Ordering::SeqCst),
                repairs.load(Ordering::SeqCst),
            ),
        )
    }

    #[tokio::test]
    async fn test_install_with_record_recovery_success_first_try() {
        // A clean install never touches the tree.
        let (result, counts) = drive_recovery(vec![fake_output(true, "")], Ok(())).await;
        assert!(result.is_ok());
        assert_eq!(counts, (1, 0));
    }

    #[tokio::test]
    async fn test_install_with_record_recovery_repairs_on_missing_record() {
        // The install aborts on a missing RECORD file, so the tree is repaired
        // and the install retried against it (#155/#183/#330). The retry must
        // NOT be a different install: --ignore-installed is gone, so the only
        // thing that changes between the two attempts is the state of the tree.
        let (result, counts) = drive_recovery(
            vec![
                fake_output(false, "error: uninstall-no-record-file"),
                fake_output(true, ""),
            ],
            Ok(()),
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(counts, (2, 1));
    }

    #[tokio::test]
    async fn test_install_with_record_recovery_bails_on_other_failure() {
        // A failure that is NOT a missing-RECORD abort bails immediately,
        // surfacing the original stderr. Nothing is repaired: the tree is not
        // implicated, so replacing it would destroy a working install over an
        // unrelated failure (a bad version pin, a network blip).
        let (result, counts) =
            drive_recovery(vec![fake_output(false, "some other pip failure")], Ok(())).await;
        let err = result.unwrap_err().to_string();
        assert!(err.contains("failed"), "lost the caller's prefix: {err}");
        assert!(
            err.contains("some other pip failure"),
            "lost pip's own reason: {err}"
        );
        assert_eq!(counts, (1, 0));
    }

    #[tokio::test]
    async fn test_install_with_record_recovery_surfaces_repair_failure() {
        // A repair that cannot run (PyPI unreachable, no bundle) must fail the
        // install with that reason rather than retrying into the same abort.
        let (result, counts) = drive_recovery(
            vec![fake_output(false, "error: uninstall-no-record-file")],
            Err(anyhow::anyhow!("pypi unreachable")),
        )
        .await;
        let err = format!("{:#}", result.unwrap_err());
        assert!(err.contains("pypi unreachable"), "{err}");
        // The install is not retried after a failed repair.
        assert_eq!(counts, (1, 1));
    }
}
