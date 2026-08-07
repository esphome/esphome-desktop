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

    // The destination itself gets the same guard as every entry written into it.
    // `Path::exists`/`create_dir_all` follow symlinks, so a `dst` that is a link
    // to a directory would read as an existing destination and the whole tree
    // would land wherever the link points — outside the managed location, with a
    // successful return. Recursing through here means child directories are
    // covered by this one call too.
    clear_mismatched_dest(dst, true)?;
    fs::create_dir_all(dst)
        .with_context(|| format!("Failed to create destination directory {dst:?}"))?;

    // Every failure below names the entry it happened on. A bundled Python tree
    // is tens of thousands of files deep, so a bare "Failed to copy file: No
    // such file or directory" leaves no way to tell which entry aborted the
    // copy — the diagnosis this function exists to make possible.
    for entry in
        fs::read_dir(src).with_context(|| format!("Failed to read source directory {src:?}"))?
    {
        let entry =
            entry.with_context(|| format!("Failed to read a directory entry in {src:?}"))?;
        let path = entry.path();
        let dest_path = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .with_context(|| format!("Failed to read file type of {path:?}"))?;

        if file_type.is_symlink() {
            copy_symlink(&path, &dest_path)?;
        } else if file_type.is_dir() {
            copy_dir_recursive(&path, &dest_path)?;
        } else {
            clear_mismatched_dest(&dest_path, false)?;
            fs::copy(&path, &dest_path)
                .with_context(|| format!("Failed to copy {path:?} to {dest_path:?}"))?;
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
    // A failure here is not fatal — the symlink call below may still succeed —
    // but it is the likely cause when that call then reports `AlreadyExists`,
    // so keep it and name it there rather than letting the real reason (EACCES,
    // a Windows handle held on the path, a half-deleted directory) be replaced
    // by its symptom. Which step failed is kept with the error: a removal we
    // attempted and a stat that never let us try are different stories, and
    // reporting the second as the first sends the reader after a call that was
    // never made.
    let blocked: Option<Blocked> = match dst.symlink_metadata() {
        Ok(meta) => remove_entry(dst, &meta.file_type())
            .err()
            .map(Blocked::Removal),
        // Nothing there is the common case and the good one: no removal needed.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        // Any other stat failure (EACCES on the parent, a Windows sharing
        // violation) means we could not even find out whether something is in
        // the way, so no removal was attempted.
        Err(e) => Some(Blocked::Stat(e)),
    };

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| link_context(dst, "symlink", &blocked))?;
    }

    #[cfg(windows)]
    {
        // Windows requires the link *type* to match the target. Take it from the
        // source link itself rather than from stat-ing what it points at: a
        // dangling or unreadable target — the very case this function exists to
        // tolerate — makes `Path::is_dir()` answer `false` for "not a directory"
        // and for "could not tell" alike, which would silently downgrade a
        // directory link to a file link and still report a successful copy.
        let src_type = src
            .symlink_metadata()
            .context("Failed to read source symlink type")?
            .file_type();
        if std::os::windows::fs::FileTypeExt::is_symlink_dir(&src_type) {
            std::os::windows::fs::symlink_dir(&target, dst)
                .with_context(|| link_context(dst, "directory symlink", &blocked))?;
        } else {
            std::os::windows::fs::symlink_file(&target, dst)
                .with_context(|| link_context(dst, "file symlink", &blocked))?;
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

/// Delete whatever `file_type` says is sitting at `dst`, using the call that can
/// actually remove it: `remove_dir_all` for a real directory, `remove_dir` for a
/// *directory symlink* on Windows (where `remove_file` cannot delete one), and
/// `remove_file` for everything else.
fn remove_entry(dst: &Path, file_type: &std::fs::FileType) -> std::io::Result<()> {
    if file_type.is_symlink() {
        match std::fs::remove_file(dst) {
            Ok(()) => Ok(()),
            Err(first) => match std::fs::remove_dir(dst) {
                // The fallback cleared it, so the first failure was the expected
                // "this is a directory link" one and says nothing. Reporting it
                // would blame a removal that ultimately worked.
                Ok(()) => Ok(()),
                Err(second) => Err(pick_removal_error(file_type, first, second)),
            },
        }
    } else if file_type.is_dir() {
        std::fs::remove_dir_all(dst)
    } else {
        std::fs::remove_file(dst)
    }
}

/// Both removals of a symlink failed; pick the one that names the real cause.
///
/// Which call was ever going to succeed depends on the kind of link, so the
/// other one's failure is noise that would misreport the reason.
#[cfg(unix)]
fn pick_removal_error(
    _file_type: &std::fs::FileType,
    remove_file_error: std::io::Error,
    _remove_dir_error: std::io::Error,
) -> std::io::Error {
    // On unix `remove_file` unlinks every symlink, directory link included, so
    // it is the call that mattered — `remove_dir` was only ever going to answer
    // `ENOTDIR`, which would replace a real EACCES/EROFS with a red herring.
    remove_file_error
}

/// See the unix twin above; on Windows the link type decides which call was the
/// one that could have worked.
#[cfg(windows)]
fn pick_removal_error(
    file_type: &std::fs::FileType,
    remove_file_error: std::io::Error,
    remove_dir_error: std::io::Error,
) -> std::io::Error {
    if std::os::windows::fs::FileTypeExt::is_symlink_dir(file_type) {
        remove_dir_error
    } else {
        remove_file_error
    }
}

/// Clear a destination entry whose *kind* disagrees with the source entry about
/// to be written there, so the write cannot follow a stale link out of the tree.
///
/// [`copy_symlink`] already replaces whatever it finds, but the destination
/// directory and the plain-file branch of [`copy_dir_recursive`] do not:
/// `Path::exists`, `create_dir_all`, and [`std::fs::copy`] all follow symlinks,
/// so a leftover link where the source now has a real directory or file would be
/// written *through* — landing the bundled tree's contents wherever the link
/// points and still returning `Ok(())`. A destination of the same kind is left
/// alone; the copy overwrites it in place as before.
fn clear_mismatched_dest(dst: &Path, want_dir: bool) -> Result<()> {
    let file_type = match dst.symlink_metadata() {
        Ok(meta) => meta.file_type(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            return Err(e).with_context(|| format!("Failed to inspect existing entry at {dst:?}"))
        }
    };

    // A symlink always goes: even one pointing at the right kind of thing would
    // redirect the write. A real entry only goes when its kind differs.
    if !file_type.is_symlink() && file_type.is_dir() == want_dir {
        return Ok(());
    }

    remove_entry(dst, &file_type)
        .with_context(|| format!("Failed to remove stale entry at {dst:?} before copying over it"))
}

/// Why a pre-existing entry at the destination is still there. Carried to
/// [`link_context`] so the message names the step that actually failed: a
/// removal we tried, or a stat that never let us try one.
enum Blocked {
    /// Something was there and [`remove_entry`] could not delete it.
    Removal(std::io::Error),
    /// `symlink_metadata` failed for a reason other than `NotFound`, so whether
    /// anything is in the way is unknown and no removal was attempted.
    Stat(std::io::Error),
}

/// Name the step that most likely blocked the symlink call, so a stale entry we
/// could not clear does not surface as a bare `AlreadyExists` with its actual
/// cause thrown away. `what` names the link kind for the platform that failed.
fn link_context(dst: &Path, what: &str, blocked: &Option<Blocked>) -> String {
    match blocked {
        Some(Blocked::Removal(e)) => format!(
            "Failed to create {what} at {dst:?}; the existing entry there could not be \
             removed first: {e}"
        ),
        Some(Blocked::Stat(e)) => format!(
            "Failed to create {what} at {dst:?}; whether anything was already there could \
             not be inspected first, so no removal was attempted: {e}"
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
        let msg = link_context(Path::new("/x/y"), "symlink", &Some(Blocked::Removal(err)));
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
    fn link_context_distinguishes_a_failed_inspection_from_a_failed_removal() {
        // A stat that failed means no removal was ever attempted. Reporting it
        // as "could not be removed" points the reader at a call that never ran.
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let msg = link_context(Path::new("/x/y"), "symlink", &Some(Blocked::Stat(err)));
        assert!(
            msg.contains("could not be inspected first"),
            "message must name the inspection, not a removal: {msg}"
        );
        assert!(
            !msg.contains("could not be removed first"),
            "no removal was attempted, so none should be blamed: {msg}"
        );
        assert!(
            msg.to_lowercase().contains("permission denied"),
            "message must carry the stat error itself: {msg}"
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

        // A tree a previous crash left half-populated, or a bundle whose layout
        // changed between releases, hands the second copy entries of the wrong
        // kind: a real file and a real directory where the source has links.
        // Both must be replaced by the source's links, not written into.
        fs::remove_file(dst.join("link.txt")).unwrap();
        fs::write(dst.join("link.txt"), b"stale regular file").unwrap();
        fs::remove_file(dst.join("versions/Current")).unwrap();
        fs::create_dir_all(dst.join("versions/Current")).unwrap();
        fs::write(dst.join("versions/Current/stale"), b"x").unwrap();

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
    fn copy_dir_recursive_replaces_stale_links_instead_of_writing_through_them() {
        // The mirror of the test above: the destination holds *links* where the
        // source now has a real directory and a real file. Following them would
        // write the bundled tree's contents outside the destination and still
        // report success, so the link must be cleared first.
        use std::fs;
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("stale-links");
        let src = base.join("src");
        let dst = base.join("dst");
        let outside = base.join("outside");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(src.join("lib")).unwrap();
        fs::write(src.join("lib/mod.py"), b"real").unwrap();
        fs::write(src.join("plain.txt"), b"real").unwrap();
        fs::create_dir_all(outside.join("lib")).unwrap();
        fs::write(outside.join("plain.txt"), b"untouched").unwrap();

        fs::create_dir_all(&dst).unwrap();
        symlink(outside.join("lib"), dst.join("lib")).unwrap();
        symlink(outside.join("plain.txt"), dst.join("plain.txt")).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(
            !fs::symlink_metadata(dst.join("lib"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "a stale directory link must be replaced by the real directory"
        );
        assert!(
            !fs::symlink_metadata(dst.join("plain.txt"))
                .unwrap()
                .file_type()
                .is_symlink(),
            "a stale file link must be replaced by the real file"
        );
        assert_eq!(fs::read_to_string(dst.join("lib/mod.py")).unwrap(), "real");
        // Nothing was written through either link.
        assert!(!outside.join("lib/mod.py").exists());
        assert_eq!(
            fs::read_to_string(outside.join("plain.txt")).unwrap(),
            "untouched"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn copy_dir_recursive_replaces_a_symlinked_destination_root() {
        // The root gets the same guard as its children: a `dst` that is a link
        // to a directory must not be written through, which would put the whole
        // Python tree outside the managed location and still return `Ok(())`.
        use std::fs;
        use std::os::unix::fs::symlink;

        let base = unique_temp_dir("linked-root");
        let src = base.join("src");
        let dst = base.join("dst");
        let outside = base.join("outside");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("real.txt"), b"hello").unwrap();
        fs::create_dir_all(&outside).unwrap();
        symlink(&outside, &dst).unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert!(
            !fs::symlink_metadata(&dst).unwrap().file_type().is_symlink(),
            "a symlinked destination root must be replaced by a real directory"
        );
        assert_eq!(fs::read_to_string(dst.join("real.txt")).unwrap(), "hello");
        assert!(
            !outside.join("real.txt").exists(),
            "nothing may be written through the destination link"
        );

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn clear_mismatched_dest_keeps_a_matching_real_entry() {
        // Only mismatches are cleared — wiping a same-kind directory would turn
        // every re-copy into a full delete-and-rewrite of the Python tree.
        use std::fs;

        let base = unique_temp_dir("keep-match");
        let _ = fs::remove_dir_all(&base);
        let dir = base.join("dir");
        fs::create_dir_all(dir.join("keep")).unwrap();
        fs::write(base.join("file.txt"), b"keep").unwrap();

        clear_mismatched_dest(&dir, true).unwrap();
        clear_mismatched_dest(&base.join("file.txt"), false).unwrap();
        // A path with nothing at it is not an error.
        clear_mismatched_dest(&base.join("absent"), false).unwrap();

        assert!(dir.join("keep").is_dir());
        assert_eq!(fs::read_to_string(base.join("file.txt")).unwrap(), "keep");

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
