//! Recursive directory copy that preserves symlinks — the mechanism the
//! bundled-Python copy in [`super`] is built on. Plain filesystem work with no
//! Python in it, so it is kept apart from the tree lifecycle that calls it.

use anyhow::{Context, Result};
use std::path::Path;

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
pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
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
    //
    // A removal that fails is not fatal here — the symlink call below may still
    // succeed — but it is the likely cause when that call then reports
    // `AlreadyExists`, so keep it and name it there rather than letting the
    // real reason (EACCES, a Windows handle held on the path, a half-deleted
    // directory) be replaced by its symptom.
    let mut removal_error: Option<std::io::Error> = None;
    match dst.symlink_metadata() {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                // A directory symlink must be removed with `remove_dir` on Windows;
                // `remove_file` works for file symlinks on all platforms. Try
                // `remove_file` first, then fall back to `remove_dir`.
                if std::fs::remove_file(dst).is_err() {
                    // Only the fallback's error is worth keeping: on a directory
                    // symlink the `remove_file` failure is expected and says
                    // nothing, so recording it when `remove_dir` then *succeeds*
                    // would blame a removal that worked for a later symlink
                    // failure with an unrelated cause (EACCES, ENOSPC).
                    removal_error = std::fs::remove_dir(dst).err();
                }
            } else if file_type.is_dir() {
                removal_error = std::fs::remove_dir_all(dst).err();
            } else {
                removal_error = std::fs::remove_file(dst).err();
            }
        }
        // Nothing there is the common case and the good one: no removal needed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // Any other stat failure (EACCES on the parent, a Windows sharing
        // violation) means we could not even find out whether something is in
        // the way, so no removal was attempted. Keep it for the same reason as
        // a failed removal: it is the likely cause if the symlink call below
        // then reports `AlreadyExists`.
        Err(e) => removal_error = Some(e),
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| link_context(dst, "symlink", &removal_error))?;
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
                .with_context(|| link_context(dst, "directory symlink", &removal_error))?;
        } else {
            std::os::windows::fs::symlink_file(&target, dst)
                .with_context(|| link_context(dst, "file symlink", &removal_error))?;
        }
    }

    // Every target this app ships to is unix or windows. A third kind would fall
    // through both arms to `Ok(())` above the removal that already happened —
    // reporting a fully successful copy of a tree silently missing the framework
    // `Current` and versioned `libpython*` links this function exists to
    // preserve. Refuse to build instead of shipping that.
    #[cfg(not(any(unix, windows)))]
    compile_error!(
        "copy_symlink has no symlink implementation for this target; a silent \
         no-op would yield a Python tree missing every symlink"
    );

    Ok(())
}

/// Name the removal that most likely blocked the symlink call, so a stale entry
/// we could not clear does not surface as a bare `AlreadyExists` with its actual
/// cause thrown away. `what` names the link kind for the platform that failed.
fn link_context(dst: &Path, what: &str, removal_error: &Option<std::io::Error>) -> String {
    match removal_error {
        Some(e) => format!(
            "Failed to create {what} at {dst:?}; the existing entry there could not be \
             removed first: {e}"
        ),
        None => format!("Failed to create {what} at {dst:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // The copy tests exercise symlink preservation, which only the unix arm of
    // `copy_symlink` can drive without elevated privileges; the temp-dir helper
    // is theirs alone, so it stays gated — otherwise a Windows build sees it
    // unused and `-D warnings` turns that into a failure.
    #[cfg(unix)]
    use crate::util::unique_temp_dir;

    #[test]
    fn link_context_names_the_blocking_removal() {
        // The removal cause is the whole point of the plumbing: without it the
        // user sees a bare `AlreadyExists` and the real reason is gone.
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = link_context(Path::new("/x/y"), "symlink", &Some(err));
        assert!(
            msg.contains("could not be removed first"),
            "message must say the stale entry blocked the link: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("permission denied"),
            "message must carry the removal error itself: {msg}"
        );
    }

    #[test]
    fn link_context_stays_quiet_when_nothing_blocked_it() {
        // No removal failed, so there is nothing to blame — mentioning one
        // would point the reader at a cause that does not exist.
        let msg = link_context(Path::new("/x/y"), "symlink", &None);
        assert!(
            !msg.contains("removed first"),
            "no removal failed, so none should be named: {msg}"
        );
        assert!(msg.contains("Failed to create symlink"));
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
    fn copy_dir_recursive_is_idempotent_over_existing_symlinks() {
        // Re-copying onto a populated destination drives the removal branch in
        // `copy_symlink`. A removal that succeeds must leave nothing behind to
        // blame: the second copy has to complete, not fail with `AlreadyExists`
        // nor report a stale-entry cause for a removal that worked.
        use std::fs;
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("idempotent");
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(src.join("versions/3.13")).unwrap();
        symlink("3.13", src.join("versions/Current")).unwrap();
        fs::write(src.join("real.txt"), b"hello").unwrap();
        symlink("real.txt", src.join("link.txt")).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();
        copy_dir_recursive(&src, &dst).expect("a second copy must overwrite, not collide");

        assert_eq!(
            fs::read_link(dst.join("versions/Current")).unwrap(),
            Path::new("3.13")
        );
        assert_eq!(
            fs::read_link(dst.join("link.txt")).unwrap(),
            Path::new("real.txt")
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
}
