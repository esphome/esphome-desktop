//! Small, dependency-free filesystem utilities.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counter to keep temp file names unique within a process even when
/// two writes to different paths race on the same millisecond/PID.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Atomically write `contents` to `path`.
///
/// A bare [`std::fs::write`] truncates the destination *before* writing the new
/// bytes. If the process is interrupted mid-write (power loss, crash, the OS
/// killing the app on quit) the file is left empty or truncated. For
/// `settings.json` that means a `serde_json` parse failure on next load and a
/// silent fall back to defaults — every user preference wiped.
///
/// This helper avoids that: it writes to a temporary file in the **same**
/// directory, flushes and `fsync`s it, then renames it over the destination.
/// `rename(2)` within a single filesystem is atomic, so a concurrent or
/// subsequent reader sees either the complete old file or the complete new
/// file — never a partial one. The temp lives in the same directory so the
/// rename never crosses a filesystem boundary (which would fall back to a
/// non-atomic copy).
pub fn atomic_write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> Result<()> {
    let path = path.as_ref();
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).ok_or_else(|| {
        anyhow!("cannot atomically write to a path without a parent directory: {path:?}")
    })?;

    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("path has no file name: {path:?}"))?
        .to_string_lossy();

    // Unique temp name in the destination directory.
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path = parent.join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));

    // Write + flush + fsync, then make sure the handle is dropped before rename
    // (matters on Windows, where an open handle blocks the rename).
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(contents.as_ref())?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        // Best-effort cleanup; the write error is what matters.
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).with_context(|| format!("failed writing temp file for {path:?}"));
    }

    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e).with_context(|| format!("failed renaming temp file into place for {path:?}"));
    }

    // Fsync the parent directory so the rename (the directory entry update) is
    // durable. Without this, a power loss right after a successful save can roll
    // back to the previous file — no corruption, but the most recent change is
    // lost. Best-effort: a no-op or EINVAL on some platforms (e.g. Windows).
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test scratch directory under the system temp dir, cleaned up on drop.
    struct TmpDir(std::path::PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "esphome-desktop-test-{}-{}-{seq}",
                std::process::id(),
                tag
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn writes_new_file() {
        let dir = TmpDir::new("new");
        let target = dir.path().join("settings.json");

        atomic_write(&target, b"hello").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    }

    #[test]
    fn overwrites_existing_file_atomically() {
        let dir = TmpDir::new("overwrite");
        let target = dir.path().join("settings.json");

        std::fs::write(&target, "old contents that are longer").unwrap();
        atomic_write(&target, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    #[test]
    fn leaves_no_temp_files_behind() {
        let dir = TmpDir::new("no-temp");
        let target = dir.path().join("settings.json");

        atomic_write(&target, b"a").unwrap();
        atomic_write(&target, b"bb").unwrap();

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        assert_eq!(entries, vec!["settings.json".to_string()]);
    }

    #[test]
    fn rejects_path_without_parent() {
        // The filesystem root has no parent — there is nowhere to stage a temp.
        let err = atomic_write(Path::new("/"), b"x");
        assert!(err.is_err());
    }

    #[test]
    fn round_trips_pretty_json() {
        let dir = TmpDir::new("json");
        let target = dir.path().join("settings.json");
        let payload = "{\n  \"port\": 6052\n}";

        atomic_write(&target, payload).unwrap();

        assert_eq!(std::fs::read_to_string(&target).unwrap(), payload);
    }
}
