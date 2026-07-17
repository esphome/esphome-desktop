//! Linux-only behavior: detecting a usable appindicator backend so the tray
//! is only offered where it can actually appear.

use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

pub fn init() {
    // Linux-specific initialization
}

/// Shared-library sonames that provide a usable appindicator backend,
/// most-preferred first. ayatana is the maintained fork; the legacy
/// `libappindicator3` names are kept for older distributions.
const APPINDICATOR_SONAMES: &[&str] = &[
    "libayatana-appindicator3.so.1",
    "libappindicator3.so.1",
    "libayatana-appindicator3.so",
    "libappindicator3.so",
];

/// Directories, relative to `APPDIR`, where a sharun-based AppImage may
/// place bundled shared libraries. `shared/lib` is sharun's default; the
/// rest cover multiarch / `usr`-prefixed layouts seen in the wild.
const APPDIR_LIB_DIRS: &[&str] = &[
    "shared/lib",
    "shared/lib/x86_64-linux-gnu",
    "shared/lib/aarch64-linux-gnu",
    "usr/lib",
    "usr/lib/x86_64-linux-gnu",
    "usr/lib/aarch64-linux-gnu",
    "usr/lib64",
    "lib",
];

/// Build the list of absolute paths where a bundled appindicator library
/// might live inside an AppImage rooted at `appdir`.
fn appindicator_candidate_paths(appdir: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(APPDIR_LIB_DIRS.len() * APPINDICATOR_SONAMES.len());
    for dir in APPDIR_LIB_DIRS {
        for name in APPINDICATOR_SONAMES {
            paths.push(appdir.join(dir).join(name));
        }
    }
    paths
}

/// Locate every bundled appindicator library that exists inside an
/// AppImage's `APPDIR`, in candidate-priority order. The caller attempts
/// each in turn so a first match that fails to load (wrong arch, broken
/// symlink, missing transitive dep) does not mask a later one that loads.
fn find_bundled_appindicators(appdir: &Path) -> Vec<PathBuf> {
    appindicator_candidate_paths(appdir)
        .into_iter()
        .filter(|p| p.exists())
        .collect()
}

/// Check if a usable appindicator library is available on this system.
///
/// The `tray-icon` crate (via `libappindicator-sys`) lazily `dlopen`s the
/// library by bare soname and will `panic!()` if it cannot be loaded. We
/// probe for it first so we can degrade gracefully instead of crashing.
///
/// On a sharun-based AppImage the bundled library is not on the loader's
/// default search path, and sharun sets `DT_RUNPATH` (which `dlopen`
/// ignores when resolving a bare soname), so the plain soname probe gets a
/// false negative — suppressing the tray even on desktops that fully
/// support it (e.g. KDE Plasma, issue #87). To handle that we locate the
/// bundled copy by absolute path and load it, leaving it resident so the
/// crate's later bare-soname `dlopen` resolves to the already-loaded object.
pub fn is_appindicator_available() -> bool {
    use std::ffi::OsStr;

    // 1. Standard probe: resolve by bare soname through the loader's
    //    default search path. Succeeds on deb/rpm/AUR installs (and any
    //    system with the library installed normally).
    for name in APPINDICATOR_SONAMES {
        if unsafe { libloading::Library::new(OsStr::new(name)) }.is_ok() {
            return true;
        }
    }

    // 2. AppImage fallback: find the bundled library by absolute path and
    //    load it. The dynamic linker matches an already-loaded object by
    //    its `DT_SONAME`, so priming it here makes `libappindicator-sys`'s
    //    later bare-soname `dlopen` succeed instead of panicking. We
    //    deliberately leak the handle (`mem::forget`) so the library stays
    //    resident for the lifetime of the process. We try every existing
    //    candidate so a first match that fails to load does not mask a
    //    later one that would have loaded successfully.
    if let Ok(appdir) = std::env::var("APPDIR") {
        let candidates = find_bundled_appindicators(Path::new(&appdir));
        if candidates.is_empty() {
            debug!("APPDIR set but no bundled appindicator library found");
        }
        for lib_path in candidates {
            match unsafe { libloading::Library::new(&lib_path) } {
                Ok(lib) => {
                    info!("Loaded bundled appindicator from {:?}", lib_path);
                    std::mem::forget(lib);
                    return true;
                }
                Err(e) => {
                    warn!(
                        "Found bundled appindicator at {:?} but it failed to load: {}",
                        lib_path, e
                    );
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;
    use std::fs;

    #[test]
    fn candidate_paths_are_all_rooted_in_appdir() {
        let appdir = Path::new("/tmp/.mount_abc");
        let paths = appindicator_candidate_paths(appdir);
        assert!(!paths.is_empty());
        assert!(paths.iter().all(|p| p.starts_with(appdir)));
        // Every candidate must end with one of the known sonames.
        assert!(paths.iter().all(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            APPINDICATOR_SONAMES.contains(&name)
        }));
    }

    #[test]
    fn candidate_paths_include_sharun_default_layout() {
        let appdir = Path::new("/opt/app");
        let paths = appindicator_candidate_paths(appdir);
        // sharun's default `shared/lib` plus the preferred ayatana soname.
        assert!(paths.contains(&appdir.join("shared/lib/libayatana-appindicator3.so.1")));
    }

    #[test]
    fn find_bundled_appindicators_locates_existing_library() {
        let appdir = unique_temp_dir("found");
        let lib_dir = appdir.join("shared/lib");
        fs::create_dir_all(&lib_dir).unwrap();
        let lib = lib_dir.join("libayatana-appindicator3.so.1");
        fs::write(&lib, b"\x7fELF").unwrap();

        assert_eq!(find_bundled_appindicators(&appdir), vec![lib]);

        let _ = fs::remove_dir_all(&appdir);
    }

    #[test]
    fn find_bundled_appindicators_returns_all_existing_in_priority_order() {
        let appdir = unique_temp_dir("multi");
        let shared = appdir.join("shared/lib");
        let usr = appdir.join("usr/lib");
        fs::create_dir_all(&shared).unwrap();
        fs::create_dir_all(&usr).unwrap();
        // A lower-priority soname in shared/lib and the preferred soname in
        // a lower-priority dir, to exercise candidate ordering.
        let shared_legacy = shared.join("libappindicator3.so.1");
        let usr_ayatana = usr.join("libayatana-appindicator3.so.1");
        fs::write(&shared_legacy, b"\x7fELF").unwrap();
        fs::write(&usr_ayatana, b"\x7fELF").unwrap();

        let found = find_bundled_appindicators(&appdir);
        assert!(found.contains(&shared_legacy));
        assert!(found.contains(&usr_ayatana));
        // shared/lib precedes usr/lib in APPDIR_LIB_DIRS.
        assert!(
            found.iter().position(|p| p == &shared_legacy)
                < found.iter().position(|p| p == &usr_ayatana)
        );

        let _ = fs::remove_dir_all(&appdir);
    }

    #[test]
    fn find_bundled_appindicators_returns_empty_when_absent() {
        let appdir = unique_temp_dir("absent");
        fs::create_dir_all(&appdir).unwrap();

        assert!(find_bundled_appindicators(&appdir).is_empty());

        let _ = fs::remove_dir_all(&appdir);
    }
}
