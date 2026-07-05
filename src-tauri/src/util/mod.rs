//! Small, dependency-free filesystem utilities.

use anyhow::{anyhow, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
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
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
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
        return Err(e)
            .with_context(|| format!("failed renaming temp file into place for {path:?}"));
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

/// Append `.{n}` to a path's file name, e.g. `dashboard.log` -> `dashboard.log.1`.
/// Keeps the original extension so rotated files stay grouped next to the live
/// log (unlike `with_extension`, which would replace `.log`).
fn numbered(path: &Path, n: usize) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{n}"));
    path.with_file_name(name)
}

/// Rename `from` → `to`, replacing `to` if it already exists.
///
/// `std::fs::rename` overwrites the destination on Unix but **fails** on Windows
/// when it exists, which would abort rotation on the second run and let the
/// caller truncate the live log again; remove the destination first so the move
/// succeeds on every platform.
fn rename_replacing(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.exists() {
        let _ = std::fs::remove_file(to);
    }
    std::fs::rename(from, to)
}

/// Create a unique, empty temp directory for a test and return its path.
/// The process id plus a per-test tag and a monotonic counter keep both
/// intra-process parallelism and two concurrent `cargo test` binaries on the
/// same host from colliding. Callers clean up after themselves (or wrap the
/// path in a guard like the `TmpDir` in this module's tests); leftovers from a
/// crashed run with a recycled pid are wiped before the directory is recreated.
#[cfg(test)]
pub(crate) fn unique_temp_dir(tag: &str) -> PathBuf {
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "esphome-desktop-test-{}-{tag}-{seq}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Rotate `path` before a fresh run, keeping up to `keep` previous copies.
///
/// The launcher redirects the dashboard child's stdout/stderr into a single
/// log file opened with [`std::fs::File::create`], which **truncates** on every
/// start — so each app launch or backend restart wiped the previous run's logs,
/// leaving nothing to inspect after a failed restart (issue #203). Calling this
/// first shifts `path` → `path.1`, `path.1` → `path.2`, … up to `path.{keep}`
/// (the oldest is discarded), so the caller can then create a fresh `path`
/// without losing history.
///
/// No-op when `keep` is 0 or `path` doesn't exist yet (first ever run). The
/// shift renames are best-effort — a single failure is skipped so one stuck
/// file can't block the rotation of the live log, which is the one that matters.
pub fn rotate_log(path: impl AsRef<Path>, keep: usize) -> Result<()> {
    let path = path.as_ref();
    if keep == 0 || !path.exists() {
        return Ok(());
    }

    // Shift the existing numbered copies up by one, oldest first, so nothing is
    // clobbered before it has been moved. `path.{keep}` is dropped when
    // `path.{keep-1}` replaces it.
    for i in (1..keep).rev() {
        let from = numbered(path, i);
        if from.exists() {
            let _ = rename_replacing(&from, &numbered(path, i + 1));
        }
    }

    let first = numbered(path, 1);
    rename_replacing(path, &first)
        .with_context(|| format!("failed rotating log {path:?} -> {first:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test scratch directory under the system temp dir, cleaned up on drop.
    struct TmpDir(std::path::PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            Self(unique_temp_dir(tag))
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
    fn rotate_log_is_noop_on_first_run() {
        let dir = TmpDir::new("rotate-first");
        let log = dir.path().join("dashboard.log");

        // Nothing to rotate yet — must not error or create stray files.
        rotate_log(&log, 3).unwrap();

        assert!(!log.exists());
        assert!(!dir.path().join("dashboard.log.1").exists());
    }

    #[test]
    fn rotate_log_shifts_existing_to_dot_one() {
        let dir = TmpDir::new("rotate-one");
        let log = dir.path().join("dashboard.log");
        std::fs::write(&log, "run-a").unwrap();

        rotate_log(&log, 3).unwrap();

        // The live log moved aside; the caller will create a fresh one.
        assert!(!log.exists());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dashboard.log.1")).unwrap(),
            "run-a"
        );
    }

    #[test]
    fn rotate_log_keeps_history_in_order_and_discards_oldest() {
        let dir = TmpDir::new("rotate-history");
        let log = dir.path().join("dashboard.log");

        // Simulate four starts, newest content written to the live log each time.
        for run in ["run1", "run2", "run3", "run4"] {
            std::fs::write(&log, run).unwrap();
            rotate_log(&log, 3).unwrap();
        }

        // keep=3 retains the three most recent rotated runs; run1 is discarded.
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("dashboard.log.1"), "run4");
        assert_eq!(read("dashboard.log.2"), "run3");
        assert_eq!(read("dashboard.log.3"), "run2");
        assert!(!dir.path().join("dashboard.log.4").exists());
    }

    #[test]
    fn rotate_log_keep_zero_is_noop() {
        let dir = TmpDir::new("rotate-zero");
        let log = dir.path().join("dashboard.log");
        std::fs::write(&log, "x").unwrap();

        rotate_log(&log, 0).unwrap();

        // With no history requested the live log is left untouched.
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "x");
        assert!(!dir.path().join("dashboard.log.1").exists());
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
