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
    if let Ok(meta) = dst.symlink_metadata() {
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            // A directory symlink must be removed with `remove_dir` on Windows;
            // `remove_file` works for file symlinks on all platforms. Try
            // `remove_file` first, then fall back to `remove_dir`.
            if std::fs::remove_file(dst).is_err() {
                let _ = std::fs::remove_dir(dst);
            }
        } else if file_type.is_dir() {
            let _ = std::fs::remove_dir_all(dst);
        } else {
            let _ = std::fs::remove_file(dst);
        }
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, dst).context("Failed to create symlink")?;
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
                .context("Failed to create directory symlink")?;
        } else {
            std::os::windows::fs::symlink_file(&target, dst)
                .context("Failed to create file symlink")?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::unique_temp_dir;

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
