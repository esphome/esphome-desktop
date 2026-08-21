//! Making bundled and system tools discoverable on `PATH`.
//!
//! The ESPHome backend we spawn inherits this process's environment verbatim,
//! so every tool it shells out to (git, patch, ccache) has to be reachable from
//! the `PATH` this process carries. This module owns the resource-dir lookups
//! for the tools we bundle, the pure `PATH` string builders, the single place
//! that mutates the environment, the crate's one `PATH` *search*
//! ([`executables_in_path`] / [`executable_on_path`], consumed by `git_check`
//! and the self-update backstop), and the `ensure_*_on_path` entry points
//! `startup/mod.rs` calls at startup.

use anyhow::{Context, Result};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use tauri::AppHandle;

/// Directory inside the bundled `git` resource that holds `git.exe`.
///
/// MinGit lays out a `cmd/git.exe` wrapper (alongside `mingw64/bin/git.exe`);
/// `cmd` is the directory Git-for-Windows recommends putting on `PATH`.
#[cfg(target_os = "windows")]
fn get_bundled_git_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let resource_dir = super::get_bundled_resource_dir(app_handle)?;
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
fn get_bundled_patch_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let resource_dir = super::get_bundled_resource_dir(app_handle)?;
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
    let resource_dir = super::get_bundled_resource_dir(app_handle)?;
    Ok(first_existing_ca_bundle(&resource_dir.join("git")))
}

/// Directory inside the bundled `ccache` resource that holds `ccache.exe`.
///
/// `prepare_bundle.sh` extracts a single static `ccache.exe` into `ccache/`.
/// Putting this dir on `PATH` lets ESPHome's ESP-IDF build discover ccache and
/// enable compiler caching automatically.
#[cfg(target_os = "windows")]
fn get_bundled_ccache_dir(app_handle: &AppHandle) -> Result<PathBuf> {
    let resource_dir = super::get_bundled_resource_dir(app_handle)?;
    Ok(resource_dir.join("ccache"))
}

/// Iterate over every executable named in `names` found on a PATH-style
/// value, in left-to-right order.
///
/// The crate's one PATH-search mechanism. `git_check` scans with it — all
/// candidates are yielded rather than stopping at the first match, so a
/// caller can keep looking past an unusable one (the macOS `/usr/bin/git`
/// stub shadowing a later real git) — and the self-update backstop asks it
/// about `dpkg`/`rpm` (`app_update::self_update_blocked`), mirroring the
/// spawn tauri-plugin-updater's install step will actually attempt. Pure
/// apart from filesystem checks — the PATH value is a parameter, so the scan
/// is unit-testable with a synthetic value and a tempdir.
pub fn executables_in_path<'a>(
    path_var: &'a OsStr,
    names: &'a [&'a str],
) -> impl Iterator<Item = PathBuf> + 'a {
    std::env::split_paths(path_var)
        // Skip empty entries (e.g. a trailing separator), which would
        // otherwise resolve to the current working directory.
        .filter(|dir| !dir.as_os_str().is_empty())
        .flat_map(move |dir| names.iter().map(move |name| dir.join(name)))
        .filter(|candidate| is_executable_file(candidate))
}

/// Whether `name` resolves to an executable regular file on the given
/// PATH-style value.
fn executable_in_path(path_var: &OsStr, name: &str) -> bool {
    executables_in_path(path_var, std::slice::from_ref(&name))
        .next()
        .is_some()
}

/// Whether `path` is a regular file this process could execute.
///
/// One `metadata` call answers both questions: a directory of the same name
/// is rejected, and on Unix an execute bit is additionally required — a
/// non-executable file named `dpkg` (or `git`) is not a tool anything here
/// could actually run, so it must not read as "present". On Windows presence
/// is the signal; the extension conveys executability.
fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// [`executable_in_path`] against this process's real `PATH`.
///
/// `var_os` (not `var`) so a non-Unicode `PATH` is searched rather than read
/// as "tool missing".
pub fn executable_on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| executable_in_path(&path, name))
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
    // Name the dir rather than a tool: the prepend path serves git, patch and
    // ccache, so a hardcoded "bundled git" would misattribute the other two.
    std::env::join_paths(entries)
        .with_context(|| format!("Failed to build PATH with {dir:?} prepended"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;

    /// Create `name` as an executable file in `dir` (mode 0755 on Unix, where
    /// the execute bit is what [`is_executable_file`] requires).
    fn create_executable(dir: &Path, name: &str) {
        let path = dir.join(name);
        std::fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn executable_in_path_finds_the_tool_and_skips_empty_entries() {
        let dir = unique_temp_dir("exe-on-path");
        create_executable(&dir, "dpkg");
        // A leading empty entry (a stray separator) must be skipped, not
        // resolved against the current directory; the real entry still hits.
        let joined = std::env::join_paths([PathBuf::new(), dir]).unwrap();
        assert!(executable_in_path(&joined, "dpkg"));
        assert!(!executable_in_path(&joined, "rpm"), "absent tool found");
    }

    #[test]
    fn executable_in_path_rejects_a_directory_of_the_same_name() {
        let dir = unique_temp_dir("exe-dir-shadow");
        std::fs::create_dir_all(dir.join("dpkg")).unwrap();
        let joined = std::env::join_paths([dir]).unwrap();
        assert!(!executable_in_path(&joined, "dpkg"));
    }

    /// A non-executable file named like the tool is not something the updater
    /// could spawn, so it must not read as "present" (Unix only — on Windows
    /// the extension is the executability signal).
    #[cfg(unix)]
    #[test]
    fn executable_in_path_requires_the_execute_bit() {
        let dir = unique_temp_dir("exe-no-x-bit");
        std::fs::write(dir.join("dpkg"), b"").unwrap();
        let joined = std::env::join_paths([dir]).unwrap();
        assert!(!executable_in_path(&joined, "dpkg"));
    }

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
}
