//! Acting on the answer: installs, channel switches, and tree repair.
//!
//! The write half of the update module. [`super::check`] decides whether there
//! is something to do; this decides what lands on disk.

use anyhow::{Context, Result};
use tauri::AppHandle;
use tracing::{debug, info, warn};

use crate::i18n::t_with;
use crate::platform;
use crate::settings::{Backend, ReleaseChannel};

use super::install::{
    install_with_record_recovery, interpreter_usable, notify_repair_incomplete,
    notify_repair_needed, probe_esphome, repair_hint, run_dev_install, run_device_builder_install,
    run_esphome_install,
};
use super::UpdateChecker;

impl UpdateChecker {
    /// Perform an update to the specified version, or install from git for dev channel.
    pub async fn update_to(
        &self,
        app_handle: &AppHandle,
        version: &str,
        channel: ReleaseChannel,
    ) -> Result<()> {
        let python_path = platform::get_python_path(app_handle)?;

        if channel == ReleaseChannel::Dev || version == "dev" {
            info!("Installing ESPHome from GitHub (dev channel)");

            // A clean --force-reinstall. If pip aborts because a dependency
            // (e.g. zeroconf) has no RECORD file, repair the tree and retry
            // against a clean copy — same broken-RECORD recovery as #155, here
            // on the dev/GitHub path (#183).
            let pp = python_path.clone();
            install_with_record_recovery(
                move || {
                    let pp = pp.clone();
                    async move { run_dev_install(&pp).await }
                },
                || self.repair_python_tree(app_handle),
                "ESPHome dev installed successfully from GitHub",
                "pip install from GitHub failed",
            )
            .await
        } else {
            info!("Updating ESPHome to version {}", version);

            // Pin the exact version and route through the shared broken-RECORD
            // recovery. A stable/beta `pip install esphome==X` uninstalls the
            // differing installed copy first, and that uninstall aborts with
            // `error: uninstall-no-record-file` when the bundled tree has a
            // missing `dist-info/RECORD` — the same failure the dev (#183) and
            // device-builder (#155) paths already recover from. Without this,
            // stable/beta was the one install path lacking that parity.
            let pp = python_path.clone();
            let version = version.to_string();
            install_with_record_recovery(
                move || {
                    let pp = pp.clone();
                    let version = version.clone();
                    async move { run_esphome_install(&pp, &version).await }
                },
                || self.repair_python_tree(app_handle),
                "ESPHome updated successfully",
                "pip install failed",
            )
            .await
        }
    }

    /// Install or upgrade the `esphome-device-builder` package from PyPI.
    /// Pass `Backend::BuilderBeta` to allow pre-releases (`pip install --pre`),
    /// `Backend::BuilderStable` for stable-only.
    pub async fn install_device_builder(
        &self,
        app_handle: &AppHandle,
        backend: Backend,
    ) -> Result<()> {
        let python_path = platform::get_python_path(app_handle)?;

        info!("Installing/upgrading esphome-device-builder ({})", backend);

        // For the stable channel, resolve the concrete latest stable version and
        // pin it so pip will *downgrade* off a newer installed beta. A plain
        // `--upgrade` without a pin never downgrades, so a beta->stable switch
        // would otherwise be a silent no-op (#200). Beta stays unpinned
        // (`--pre --upgrade`), which already moves forward to the latest
        // pre-release. If the PyPI lookup fails we fall back to the unpinned
        // upgrade rather than block the install entirely.
        let version = if backend == Backend::BuilderStable {
            match self.check_device_builder(Backend::BuilderStable).await {
                Ok(v) => Some(v),
                Err(e) => {
                    warn!(
                        "Could not resolve latest stable device-builder version; \
                         falling back to unpinned upgrade (may not downgrade a beta): {}",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        // A clean upgrade, which uninstalls the old copy normally. Only if pip
        // aborts on a missing RECORD file (#155) do we repair the tree and
        // retry against a clean copy.
        let pp = python_path.clone();
        install_with_record_recovery(
            move || {
                let pp = pp.clone();
                let version = version.clone();
                async move { run_device_builder_install(&pp, backend, version.as_deref()).await }
            },
            || self.repair_python_tree(app_handle),
            "esphome-device-builder installed/upgraded successfully",
            "pip install esphome-device-builder failed",
        )
        .await
    }

    /// Repair a broken managed Python tree by re-copying the bundled one.
    ///
    /// Every platform ships a pristine copy of the tree inside the app and
    /// keeps a working copy in app data (#335), so the one repair everywhere is
    /// a local file copy: free, offline, and the same path that already heals
    /// the tree at every release.
    async fn repair_python_tree(&self, app_handle: &AppHandle) -> Result<()> {
        info!("Repairing the ESPHome install by re-copying the bundled Python tree");
        let app = app_handle.clone();
        tokio::task::spawn_blocking(move || {
            platform::ensure_user_python(&app, platform::RefreshReason::Repair)
        })
        .await
        .context("Bundled-Python refresh task panicked or was cancelled")?
    }

    /// Check the bundled tree with a real ESPHome command at startup and repair
    /// it if it is broken (#330).
    ///
    /// This exists because the damage outlives the bug that caused it. Removing
    /// the `--ignore-installed` fallback stops us orphaning files from now on,
    /// but every user it already hit still has the orphan on disk, and nothing
    /// else would ever clear it: their next update can succeed and still leave
    /// the stale directory sitting there breaking every compile. So look for the
    /// damage directly rather than waiting for an install to fail.
    ///
    /// Never blocks the launch: a probe that cannot run, an exhausted attempt
    /// budget, or a failed repair all continue to start the app. But a tree left
    /// broken is never silent — every compile will fail, and the user is the only
    /// one who can act on it, so [`notify_repair_needed`] tells them. Must run
    /// before the daemon starts: a running backend holds the packages open, and
    /// it would be serving a broken tree anyway.
    pub async fn repair_python_tree_if_broken(&self, app_handle: &AppHandle) {
        let python_path = match platform::get_python_path(app_handle) {
            Ok(p) => p,
            Err(e) => {
                warn!("Skipping ESPHome health probe; no Python found: {e:#}");
                return;
            }
        };

        // `get_python_path` falls back to a bare system `python3` in development
        // builds with no bundle. That interpreter fails the probe simply because
        // ESPHome is not installed in it, which is not damage and not ours to
        // repair; probing it would only produce a notification telling a
        // developer their install is broken.
        if !platform::is_managed_python_tree(&python_path) {
            debug!("Skipping ESPHome health probe; {python_path:?} is not a managed tree");
            return;
        }

        let python_parent_dir = match platform::get_python_parent_dir(app_handle) {
            Ok(d) => d,
            Err(e) => {
                warn!("Skipping ESPHome health probe; no local data dir: {e:#}");
                return;
            }
        };

        let detail = match probe_esphome(&python_path).await {
            Ok(None) => {
                debug!("ESPHome health probe passed");
                platform::clear_repair_count(&python_parent_dir);
                return;
            }
            Ok(Some(detail)) => detail,
            // The probe could not run at all. That does NOT mean the interpreter
            // is broken: the probe also needs a writable temp dir and somewhere
            // to put a config, so a full disk fails it just as well. Ask the
            // interpreter directly rather than inferring, on every platform —
            // acting on the inference would either delete a working tree we
            // cannot re-copy onto a full disk, or tell a user their install is
            // damaged when it is their disk. Both are worse than doing nothing.
            //
            // `interpreter_is_usable` is the right question to ask because it
            // spawns nothing but the interpreter, so none of those environment
            // failures can reach it.
            //
            // Deliberately not left to `ensure_user_python`'s own
            // `interpreter_is_usable` wipe: that only runs when `needs_copy` is
            // true, so a tree that broke without an app update keeps a matching
            // marker and is never reached.
            Err(e) => {
                match interpreter_usable(&python_path).await {
                    Ok(true) => {
                        warn!(
                            "ESPHome health probe could not run, but the interpreter itself is \
                             fine, so this is the environment rather than a tree we can repair: \
                             {e:#}"
                        );
                        return;
                    }
                    // Nothing established that the interpreter is fine, so do not
                    // act as though it had — in either direction.
                    Err(join) => {
                        warn!(
                            "Could not check whether the interpreter is usable ({join}), so the \
                             tree is being left alone. The probe said: {e:#}"
                        );
                        return;
                    }
                    Ok(false) => {}
                }

                // The interpreter really is wedged. A bundle re-copy fixes that
                // and needs nothing from the broken one, so fall through to the
                // repair.
                format!("the interpreter could not run the health probe: {e:#}")
            }
        };

        if !platform::may_repair_tree(&python_parent_dir) {
            warn!(
                "ESPHome install looks broken but the repair budget is spent, so it is being \
                 left alone. Probe said: {detail}"
            );
            // Not `repair_budget_left`: we were just refused, and an
            // unwritable counter refuses while still reading under the bound.
            notify_repair_needed(
                app_handle,
                t_with(
                    "update.repair_incomplete",
                    &[("hint", &repair_hint(&python_parent_dir, false))],
                ),
            );
            return;
        }

        warn!("ESPHome install is broken; repairing it. Probe said: {detail}");
        if let Err(e) = self.repair_python_tree(app_handle).await {
            warn!("ESPHome repair failed: {e:#}");
            notify_repair_needed(
                app_handle,
                t_with(
                    "update.repair_failed",
                    &[
                        ("error", &e.to_string()),
                        (
                            "hint",
                            &repair_hint(
                                &python_parent_dir,
                                platform::repair_budget_left(&python_parent_dir),
                            ),
                        ),
                    ],
                ),
            );
            return;
        }

        // Confirm the repair with the same probe that condemned the tree, so a
        // repair that did not actually fix anything says so rather than being
        // reported as a success.
        match probe_esphome(&python_path).await {
            Ok(None) => {
                info!("ESPHome install repaired");
                platform::clear_repair_count(&python_parent_dir);
            }
            Ok(Some(detail)) => {
                warn!("ESPHome install still broken after the repair: {detail}");
                notify_repair_incomplete(app_handle, &python_parent_dir);
            }
            // The repair ran but we could not confirm it. Treat that as
            // unrepaired rather than as success: the probe already proved the
            // tree was broken, so "we cannot tell" is much closer to "still
            // broken" than to "fine", and staying quiet here would make an
            // unverifiable repair the one outcome the user is never told about.
            // Leaving the counter alone is deliberate for the same reason — an
            // unconfirmed repair has not earned back its budget.
            Err(e) => {
                warn!("Could not re-check the ESPHome install after the repair: {e:#}");
                notify_repair_incomplete(app_handle, &python_parent_dir);
            }
        }
    }

    /// Switch to a new release channel by installing the appropriate version.
    /// Returns Ok(()) on success.
    pub async fn switch_channel(
        &self,
        app_handle: &AppHandle,
        channel: ReleaseChannel,
    ) -> Result<()> {
        match channel {
            ReleaseChannel::Stable => {
                // Install the latest stable version
                let latest = self
                    .check(ReleaseChannel::Stable)
                    .await?
                    .context("Could not determine latest stable version")?;
                self.update_to(app_handle, &latest, ReleaseChannel::Stable)
                    .await
            }
            ReleaseChannel::Beta => {
                // Install the latest beta version
                let latest = self
                    .check(ReleaseChannel::Beta)
                    .await?
                    .context("Could not determine latest beta version")?;
                self.update_to(app_handle, &latest, ReleaseChannel::Beta)
                    .await
            }
            ReleaseChannel::Dev => {
                // Install from GitHub
                self.update_to(app_handle, "dev", ReleaseChannel::Dev).await
            }
        }
    }
}
