//! The base-package manifest and the package reset built on it (#330).
//!
//! `.base-packages` records, at build time, what ships with the interpreter
//! itself; everything else in the swept directories is ours to delete and
//! reinstall. This file owns parsing the manifest, deciding which trees the
//! reset may touch at all, and the wipe itself. #335 replaces the whole
//! subsystem with a pristine-copy refresh once Windows runs from app data.

use super::{get_bundled_resource_dir, get_data_dir, get_python_path};
use anyhow::{Context, Result};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use tauri::AppHandle;

/// Filename of the manifest listing everything that ships with the interpreter
/// itself. Written into the tree at build time by
/// `build-scripts/prepare_bundle.sh`; read by [`wipe_installed_packages`].
pub(super) const BASE_MANIFEST: &str = ".base-packages";

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

    // pip must be spared, or the reset deletes the one thing it reinstalls with
    // and the tree can never be repaired again. The generator asserts this too,
    // but that runs on a build machine; this runs against a file that has since
    // been copied, shipped, and sat on a user's disk for months, and is the last
    // check standing between a bad manifest and `remove_dir_all`. `keep_key` has
    // already folded `pip-X.Y.dist-info` down to `pip`, so this sees either.
    if !manifest
        .keep
        .iter()
        .any(|keep| keep.file_name() == Some(OsStr::new("pip")))
    {
        anyhow::bail!("{BASE_MANIFEST} does not name pip; refusing to use it");
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
/// signed `.app`, a squashfs AppImage mount) and so [`super::ensure_user_python`]
/// keeps
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

    if is_resettable_tree(root, &user_root, || {
        Ok(get_bundled_resource_dir(app_handle)?.join("python"))
    })? {
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
///
/// `resource_root` is resolved lazily because most answers do not need it, and
/// resolving it can fail — a dev build with no bundled resources, say. Asking
/// eagerly would let that failure refuse a reset of the copy we own, on the
/// strength of a directory the decision never depended on.
fn is_resettable_tree(
    root: &Path,
    user_root: &Path,
    resource_root: impl FnOnce() -> Result<PathBuf>,
) -> Result<bool> {
    // The copy in the app data dir is ours everywhere.
    if root == user_root {
        return Ok(true);
    }
    // Everywhere else the resource tree is read-only by design — a signed `.app`
    // bundle, or a squashfs AppImage mount — so writing to it is damage, not
    // repair, and there is no need to find out where it is.
    if !cfg!(target_os = "windows") {
        return Ok(false);
    }
    // On Windows there is no copy: `ensure_user_python` returns early and the
    // backend runs straight out of the install dir, an ordinary per-user
    // writable directory the installer wrote. So the resource tree is the live
    // tree there, and repairing it is the whole point.
    Ok(root == resource_root()?)
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

#[cfg(test)]
mod tests {
    use super::super::tests::interpreter_in_tree;
    use super::*;
    use crate::util::unique_temp_dir;

    /// site-packages path used by the fake trees below. Its literal spelling
    /// does not matter: the manifest names the directories to sweep, so nothing
    /// in the reset infers this layout.
    const TEST_PURELIB: &str = "lib/python3.13/site-packages";

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
        let resources = || Ok(resource_root.to_path_buf());

        // The copy in the app data dir is ours to repair, everywhere.
        assert!(is_resettable_tree(user_root, user_root, resources).unwrap());

        // The shipped resource tree is a legitimate target only on Windows,
        // where it IS the live tree. Everywhere else `get_python_path` only
        // returns it because `ensure_user_python`'s copy failed (it is
        // log-and-continue at startup), and deleting inside a signed `.app` or a
        // read-only AppImage mount turns a transient failure into a reinstall.
        assert_eq!(
            is_resettable_tree(resource_root, user_root, resources).unwrap(),
            cfg!(target_os = "windows"),
            "the bundled resource tree is resettable on Windows and nowhere else"
        );

        // Anything else — a system Python, a user's own tree — is never ours.
        assert!(
            !is_resettable_tree(Path::new("/usr/local/lib/python3.13"), user_root, resources)
                .unwrap()
        );
        assert!(
            !is_resettable_tree(Path::new("/data/io.esphome.builder"), user_root, resources)
                .unwrap()
        );
    }

    #[test]
    fn the_resource_tree_is_not_resolved_when_the_answer_does_not_need_it() {
        // Resolving it can fail — a dev build with no bundled resources — and
        // asking eagerly would let that refuse a reset of the copy we own, on
        // the strength of a directory the decision never depended on.
        let user_root = Path::new("/data/io.esphome.builder/python");
        let asked = std::cell::Cell::new(false);
        let resources = || {
            asked.set(true);
            Ok(PathBuf::from("/never/needed"))
        };

        assert!(is_resettable_tree(user_root, user_root, resources).unwrap());
        assert!(
            !asked.get(),
            "resolved the resource tree to answer 'that is ours'"
        );

        // And a resolution failure cannot refuse a tree we own.
        let boom = || anyhow::bail!("no bundled resources in this build");
        assert!(is_resettable_tree(user_root, user_root, boom).unwrap());
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
    fn parse_base_manifest_rejects_a_manifest_that_does_not_name_pip() {
        // The generator refuses to emit one, but that runs on a build machine.
        // This runs against a file that has been copied, shipped and sat on disk
        // since — and it is the last thing between a bad manifest and a
        // recursive delete of the pip the repair reinstalls with.
        let err = parse_base_manifest(&format!(
            "sweep {TEST_PURELIB}\nsweep bin\nkeep {TEST_PURELIB}/setuptools\nkeep bin/python3\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("pip"), "{err}");

        // Named either way round: `keep_key` folds the versioned metadata dir
        // down to the distribution name.
        assert!(parse_base_manifest(&format!(
            "sweep {TEST_PURELIB}\nsweep bin\nkeep {TEST_PURELIB}/pip-26.1.2.dist-info\nkeep bin/python3\n"
        ))
        .is_ok());
    }

    #[test]
    fn parse_base_manifest_rejects_a_sweep_dir_with_nothing_kept() {
        // Checking the keep set only globally would accept this: `bin` names
        // entries, so the manifest looks populated, while site-packages is swept
        // clean — taking the interpreter's own pip with it.
        let err = parse_base_manifest(&format!(
            "sweep {TEST_PURELIB}\nsweep bin\nkeep bin/python3\nkeep bin/pip\n"
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains(TEST_PURELIB), "{err}");

        // Both covered is fine.
        assert!(parse_base_manifest(&fake_manifest()).is_ok());
    }

    #[test]
    fn parse_base_manifest_rejects_an_unknown_verb() {
        // A verb we don't understand means the file was written by something
        // that disagrees with us about the format; deleting on that basis is
        // not safe.
        assert!(parse_base_manifest("sweep bin\nkeep bin/python3\ndelete bin/pip3\n").is_err());
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
}
