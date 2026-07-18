//! Spawning and supervising child processes for the bundled Python: bounded
//! execution with capture, python environment isolation, window suppression,
//! and the Windows job-object and console-signal plumbing. Everything
//! pip-specific composes these from [`super::pip`].

use std::ffi::OsStr;
use std::path::Path;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

#[cfg(target_os = "windows")]
use ::windows::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};

/// Maximum length of pip stderr included in a failure error message. pip's
/// resolver and progress output can run to many kilobytes; the actionable
/// failure reason is almost always at the tail, so we truncate to the last
/// N bytes to keep log lines (and downstream UI surfaces) bounded.
const PIP_STDERR_TAIL_BYTES: usize = 4096;

/// Return `s` trimmed and truncated to the last [`PIP_STDERR_TAIL_BYTES`]
/// bytes, with a marker line if anything was dropped. Backs up to a UTF-8
/// char boundary so the result is always valid `str`.
pub(super) fn tail_for_log(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= PIP_STDERR_TAIL_BYTES {
        return trimmed.to_string();
    }
    let mut start = trimmed.len() - PIP_STDERR_TAIL_BYTES;
    while start < trimmed.len() && !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "...(stderr truncated to last {} bytes)\n{}",
        PIP_STDERR_TAIL_BYTES,
        &trimmed[start..]
    )
}

/// How often [`run_bounded`] checks whether the child has exited. Small enough
/// that a deadline fires promptly, large enough that polling costs nothing: even
/// the five-minute pip bound is only a few thousand `try_wait` calls.
const CHILD_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// How long [`run_bounded`] waits for a reader thread to drain a pipe after the
/// child is done with it.
///
/// Normally instant: the child closed its end, so the reader sees EOF and
/// returns. This bounds the case where it does not — a grandchild that inherited
/// the pipe and outlived the kill — where waiting would mean the deadline never
/// fires at all. Generous enough that a merely slow reader is never cut short.
const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How a child bounded by [`run_bounded`] finished.
pub(super) enum BoundedRun {
    /// It exited on its own, within the deadline.
    Exited(std::process::Output),
    /// It outlived the deadline and was killed. Its stderr survives the kill —
    /// for a hung install that partial output is the only diagnostic there is.
    /// stdout is drained too, since that is what keeps the child off a full pipe,
    /// but not kept: no caller has wanted a killed child's stdout.
    TimedOut { stderr: Vec<u8> },
}

/// Run an already-configured `cmd` to completion, killing it if it outlives
/// `timeout`.
///
/// The caller owns the policy — which interpreter, which isolation, which pipes,
/// and what any of the outcomes mean. This owns the part that is easy to get
/// subtly wrong and expensive to get wrong twice: a child whose output fills a
/// pipe buffer (~64 KiB) blocks on `write` until someone reads the other end, so
/// the pipes must be drained on their own threads or the child outlives the very
/// deadline meant to bound it.
///
/// Waiting on those readers is itself bounded, which is less obvious and just as
/// load-bearing. A pipe reaches EOF only when *every* writer closes it, and
/// killing a child does not kill grandchildren that inherited its fds — pip
/// routinely spawns build backends. So a reader can still be blocked long after
/// its child is dead, and joining it unconditionally would let a surviving
/// grandchild hold this call open past the deadline it exists to enforce. The
/// bytes are accumulated where this function can reach them without the reader's
/// cooperation, so giving up on a stuck reader costs the tail of the output
/// rather than the guarantee.
pub(super) fn run_bounded(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> std::io::Result<BoundedRun> {
    use std::io::Read;
    use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    /// A reader thread, and the bytes it has accumulated so far.
    struct Drain {
        /// Shared rather than returned from the thread, so a reader still stuck
        /// on a pipe someone else holds open cannot keep its bytes hostage.
        buf: Arc<Mutex<Vec<u8>>>,
        done: Receiver<()>,
    }

    fn drain<R: Read + Send + 'static>(what: &'static str, handle: Option<R>) -> Option<Drain> {
        handle.map(|mut h| {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let thread_buf = Arc::clone(&buf);
            let (tx, done) = channel();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 8192];
                loop {
                    match h.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => thread_buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                        // Keep whatever arrived before the failure, and say the
                        // read broke. Silently returning a short buffer would
                        // surface as "pip install failed: " with nothing after
                        // it, which reads as a child that printed nothing rather
                        // than output we lost.
                        Err(e) => {
                            tracing::warn!("Lost part of a child's {what}: {e}");
                            break;
                        }
                    }
                }
                let _ = tx.send(());
            });
            Drain { buf, done }
        })
    }

    fn collect(what: &str, drain: Option<Drain>) -> Vec<u8> {
        let Some(drain) = drain else {
            return Vec::new();
        };
        match drain.done.recv_timeout(DRAIN_GRACE) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => tracing::warn!(
                "A child's {what} reader did not finish within {DRAIN_GRACE:?}; something that \
                 inherited the pipe is still holding it open. Returning what arrived."
            ),
            // The sender is dropped without sending only if the thread unwound.
            Err(RecvTimeoutError::Disconnected) => {
                tracing::warn!("The reader for a child's {what} panicked; returning what arrived.")
            }
        }
        // Recover a poisoned lock rather than unwrapping it. A reader that
        // panics does so inside `extend_from_slice`, i.e. holding this guard, so
        // the `Disconnected` arm above and an `unwrap()` here would contradict
        // each other: the arm promises to return what arrived, and the unwrap
        // would panic out of `run_bounded` before it could. The bytes are a
        // `Vec<u8>` with no invariant to break, so there is nothing for the
        // poison to protect.
        let mut buf = drain.buf.lock().unwrap_or_else(|p| p.into_inner());
        std::mem::take(&mut *buf)
    }

    // Unix: put the child in its own process group so the whole tree can be
    // signalled on the bound (matches `daemon::start_inner`). Windows: nothing
    // here; the per-call job is created and assigned after spawn. Do NOT touch
    // creation flags on Windows -- the callers set CREATE_NO_WINDOW and
    // `creation_flags` overwrites rather than accumulates.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd.spawn()?;
    // Tie the child's descendants to this call so a survivor -- a pip PEP 517
    // build backend that inherited the pipe, say -- can't outlive the bound.
    // Dropped on every return below (including a `?`), so it is also the
    // backstop the old "any early exit must reap the child" note described, now
    // for the whole tree rather than just the direct child.
    let reaper = Reaper::new(&child);
    let stdout_reader = drain("stdout", child.stdout.take());
    let stderr_reader = drain("stderr", child.stderr.take());

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Reap the tree BEFORE collect: killing it closes the inherited
                // pipe so the readers hit EOF at once instead of each costing a
                // full DRAIN_GRACE. The leader has already exited; this takes
                // any descendants it left holding the pipe.
                reaper.kill_tree();
                return Ok(BoundedRun::Exited(std::process::Output {
                    status,
                    stdout: collect("stdout", stdout_reader),
                    stderr: collect("stderr", stderr_reader),
                }));
            }
            Ok(None) => {}
            Err(e) => {
                reaper.kill_tree();
                // Fallback for a failed Windows job assignment (the reaper is a
                // no-op then); redundant-but-harmless where `killpg` already
                // took the leader.
                let _ = child.kill();
                let _ = child.wait();
                let _ = collect("stdout", stdout_reader);
                let _ = collect("stderr", stderr_reader);
                return Err(e);
            }
        }
        if Instant::now() >= deadline {
            reaper.kill_tree();
            let _ = child.kill();
            let _ = child.wait();
            // The tree kill above closed the pipe, so draining the stdout reader
            // returns at once; a descendant that left the group (`setsid` /
            // CREATE_NEW_PROCESS_GROUP) is bounded by DRAIN_GRACE, not joined.
            let _ = collect("stdout", stdout_reader);
            return Ok(BoundedRun::TimedOut {
                stderr: collect("stderr", stderr_reader),
            });
        }
        std::thread::sleep(CHILD_POLL_INTERVAL);
    }
}

/// Reaps a bounded child's whole process tree, so a descendant that inherited
/// the child's pipe cannot outlive the bounded call. Held by [`run_bounded`]
/// for the duration of the call; [`Reaper::kill_tree`] does the kill and `Drop`
/// is the backstop for the `?`-early-return paths.
///
/// Unix uses the child's process group (`process_group(0)` gives pgid == pid);
/// Windows uses a per-call kill-on-close job the child is assigned to.
struct Reaper {
    #[cfg(unix)]
    pgid: i32,
    #[cfg(windows)]
    job: Option<JobHandle>,
    /// So the kill fires exactly once: the explicit `kill_tree()` on each exit
    /// path does the work, and the `Drop` below is then a no-op. Load-bearing on
    /// Unix -- see the pgid-reuse note on `kill_tree`; a second `killpg` from
    /// `Drop` would widen that window for no benefit.
    killed: std::cell::Cell<bool>,
}

impl Reaper {
    #[cfg(unix)]
    fn new(child: &std::process::Child) -> Self {
        // pgid == pid because the child was spawned with `process_group(0)`.
        Reaper {
            pgid: child.id() as i32,
            killed: std::cell::Cell::new(false),
        }
    }

    #[cfg(windows)]
    fn new(child: &std::process::Child) -> Self {
        use std::os::windows::io::AsRawHandle;
        let job = create_kill_on_close_job();
        // Accept the small CreateProcess->assign race (a descendant spawned in
        // that window escapes the job), exactly as `daemon::start_inner` does:
        // closing it needs CREATE_SUSPENDED + a thread handle std does not
        // expose, and the callers spawn Python, which spawns nothing that early.
        // `assign_process_to_job` / `create_kill_on_close_job` log the Win32
        // cause; this logs the consequence.
        let covered = job
            .as_ref()
            .is_some_and(|j| assign_process_to_job(j.0, child.as_raw_handle()));
        if !covered {
            tracing::warn!(
                "A bounded child is not covered by a kill-on-close job; a descendant \
                 it spawns may outlive the call"
            );
        }
        Reaper {
            job,
            killed: std::cell::Cell::new(false),
        }
    }

    /// Kill the child and every descendant still in its group/job. Idempotent:
    /// the first call kills, later calls (e.g. from `Drop`) are no-ops.
    fn kill_tree(&self) {
        if self.killed.replace(true) {
            return;
        }
        #[cfg(unix)]
        {
            use nix::sys::signal::{killpg, Signal};
            use nix::unistd::Pid;
            // SIGKILL, not the daemon's graceful SIGTERM: this is the bound
            // firing, and the point is that nothing survives it.
            //
            // On the Exited path the leader is already reaped, so in principle
            // its pgid could be recycled before this runs -- but only once the
            // group is EMPTY, i.e. exactly when there is nothing left to kill.
            // While any descendant survives (the case this exists for) the group
            // is non-empty, so the kernel keeps the pgid reserved and the signal
            // reaches our tree. The daemon accepts the same class of window.
            let _ = killpg(Pid::from_raw(self.pgid), Signal::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(job) = &self.job {
            use ::windows::Win32::System::JobObjects::TerminateJobObject;
            // SAFETY: `job.0` is a live job handle owned by this Reaper.
            unsafe {
                let _ = TerminateJobObject(job.0, 1);
            }
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        self.kill_tree();
        #[cfg(windows)]
        if let Some(job) = &self.job {
            use ::windows::Win32::Foundation::CloseHandle;
            // SAFETY: close the sole handle to this per-call job exactly once.
            unsafe {
                let _ = CloseHandle(job.0);
            }
        }
    }
}

/// Spawn the given Python interpreter with `args` and capture its output,
/// killing it if it outlives `timeout`.
///
/// The unbounded [`run_python_capture`] is right for callers who are already
/// waiting on something else. It is wrong on the launch path: a child that never
/// exits there means the backend never starts and the tray never says why. This
/// module already draws that line for `pip install` — "bounding it prevents a
/// stalled network from hanging app startup indefinitely" — and the same
/// reasoning applies to anything else we make a user wait behind.
pub(super) fn run_python_capture_bounded<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
    timeout: std::time::Duration,
) -> std::io::Result<std::process::Output> {
    let mut cmd = python_command(python, args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    match run_bounded(cmd, timeout)? {
        BoundedRun::Exited(output) => Ok(output),
        // Carry what it managed to say. `BoundedRun` goes to the trouble of
        // draining a killed child precisely so this exists; dropping it here
        // would reduce a hung probe to "timed out after 60s" with no hint as to
        // what it was doing, which is the whole question at that point.
        BoundedRun::TimedOut { stderr } => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!(
                "timed out after {timeout:?}; partial stderr: {}",
                tail_for_log(&String::from_utf8_lossy(&stderr))
            ),
        )),
    }
}

/// The command every Python we spawn is built from: the given interpreter and
/// args, isolated from user site-packages (see [`isolate_python_command`]), with
/// no console window on Windows.
///
/// One home for that setup, so "every Python we spawn is isolated" is a property
/// of the builder rather than something each caller has to remember.
pub(super) fn python_command<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::process::Command {
    let mut cmd = std::process::Command::new(python);
    cmd.args(args);
    isolate_python_command(&mut cmd);
    configure_no_window_command(&mut cmd);
    cmd
}

/// Spawn the given Python interpreter with `args`, suppress the console
/// window on Windows, isolate it from user site-packages (see
/// [`isolate_python_command`]), and capture its output. It adds no *flags* of
/// its own (callers pass exactly the flags they need, `-I` included or not),
/// and callers keep their own policy for exit status, logging, and
/// stdout/stderr interpretation.
///
/// Unbounded; see [`run_python_capture_bounded`] for callers on the launch path.
pub fn run_python_capture<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::io::Result<std::process::Output> {
    python_command(python, args).output()
}

/// [`run_python_capture`], returning the trimmed stdout on a successful exit
/// and `None` on a non-zero exit. stderr is captured but not returned, so
/// callers that need it (or the exit status) should use
/// [`run_python_capture`] directly.
pub fn run_python_capture_stdout<S: AsRef<OsStr>>(
    python: &Path,
    args: impl IntoIterator<Item = S>,
) -> std::io::Result<Option<String>> {
    let output = run_python_capture(python, args)?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

/// Env that keeps the managed interpreter on its own tree.
///
/// The bundled Python is a plain (non-venv) install, so `site.py` runs
/// `addusersitepackages()` before `addsitepackages()` and the per-user site
/// directory (`~/.local/lib/pythonX.Y/site-packages`, or
/// `%APPDATA%\Python\PythonXY\site-packages` on Windows) lands on `sys.path`
/// AHEAD of our own `site-packages`. Anyone who has ever run `pip install
/// --user` against a same-minor system Python therefore shadows our pinned
/// dependencies with theirs, and the backend dies at import (#318). The
/// ambient `PYTHON*` vars can redirect the interpreter just as effectively, so
/// drop them too.
///
/// This is an env var rather than a `-s` flag so it also reaches the processes
/// the backend spawns for itself (esptool, PlatformIO, compilers), which run
/// against the same tree and have the same exposure. venvs already ignore user
/// site, so inheriting it costs them nothing.
pub(super) const PYTHON_ISOLATION_SET: [(&str, &str); 1] = [("PYTHONNOUSERSITE", "1")];

/// Ambient vars that can redirect the interpreter off its own tree just as
/// effectively as user site. See [`PYTHON_ISOLATION_SET`].
pub(super) const PYTHON_ISOLATION_REMOVE: [&str; 3] = ["PYTHONPATH", "PYTHONHOME", "PYTHONSTARTUP"];

/// Point the managed interpreter at its own tree only, per
/// [`PYTHON_ISOLATION_SET`].
pub fn isolate_python_command(cmd: &mut std::process::Command) {
    for (k, v) in PYTHON_ISOLATION_SET {
        cmd.env(k, v);
    }
    for k in PYTHON_ISOLATION_REMOVE {
        cmd.env_remove(k);
    }
}

/// [`isolate_python_command`] for a tokio::process::Command.
///
/// tokio's `Command` is a `std::process::Command` plus a `kill_on_drop` flag;
/// its env methods forward straight to the inner command, and `spawn` runs that
/// same command. So editing it through `as_std_mut` is what tokio would do
/// anyway, and the two variants cannot drift apart.
pub fn isolate_python_tokio_command(cmd: &mut tokio::process::Command) {
    isolate_python_command(cmd.as_std_mut());
}

/// Configure std::process::Command to not create a console window on Windows
pub fn configure_no_window_command(cmd: &mut std::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Configure tokio::process::Command to not create a console window on Windows
pub fn configure_no_window_tokio_command(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW.0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Configure the daemon child's creation flags on Windows: no console window
/// AND a new process group. The new process group makes the child its own
/// group leader (pgid == pid) so we can later deliver a graceful
/// `CTRL_BREAK_EVENT` to it (and its descendants) for shutdown via
/// `send_ctrl_break`. Sets both flags in one call so neither overwrites the
/// other. No-op on non-Windows (Unix uses `process_group(0)` instead).
pub fn configure_daemon_tokio_command(cmd: &mut tokio::process::Command) {
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags((CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP).0);
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = cmd;
    }
}

/// Tie a spawned child's lifetime to ours on Windows via a kill-on-close job
/// object. Returns `true` if the child was assigned to the job.
///
/// Every graceful shutdown path we have — `send_ctrl_break`, then
/// `TerminateProcess` as the fallback — only runs when the desktop gets to run
/// code. None of it runs when the NSIS uninstaller force-kills us, when we
/// crash, or when the user ends the task from Task Manager. `kill_on_drop` is
/// no help either: the normal quit path calls `std::process::exit()`, which
/// skips `Drop`. The backend is then orphaned, keeping `python.exe` in the
/// app-data tree — and every file its compile subtree touches, the install
/// dir's `git.exe` included — open. `ensure_user_python`'s next refresh or
/// repair cannot `remove_dir_all` a tree with open files, and the uninstaller
/// cannot remove a held `git.exe`, so the orphan strands both trees and breaks
/// the next launch.
///
/// A job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` closes that gap
/// without needing any cooperation from the dying process: when the last
/// handle to the job goes away, Windows terminates everything in it. The
/// kernel closes our handles however we exit, so this holds for a crash or a
/// force-kill just as much as for a clean quit. Descendants inherit job
/// membership, so the backend's compiler and git children are covered too.
///
/// The job deliberately holds only the daemon child, never the desktop process
/// itself. The updater spawns the NSIS installer as our child and then exits;
/// a job that included us would kill that installer mid-update.
///
/// Nested jobs have been supported since Windows 8, so already being inside
/// someone else's job (a launcher, a test runner) does not defeat this outright
/// the way it would have before, when a second assignment always failed. That
/// is not a guarantee of success: assignment can still fail, for instance if
/// the job hierarchy can't be formed. Hence best-effort, and hence the caller
/// gets told whether it worked rather than being allowed to assume it.
///
/// This is a floor, not a replacement for the graceful path: `stop()` still
/// sends `CTRL_BREAK_EVENT` first and gives the backend its full shutdown
/// window. The job only decides what happens to a child that outlives us.
#[cfg(target_os = "windows")]
pub fn assign_to_kill_on_close_job(process: std::os::windows::io::RawHandle) -> bool {
    // The process-wide OnceLock job (never closed, see `kill_on_close_job`).
    match kill_on_close_job() {
        Some(job) => assign_process_to_job(job, process),
        None => false,
    }
}

/// Assign a live child process to `job`, warning with the Win32 cause and
/// returning `false` on failure. Shared by the process-wide singleton
/// ([`assign_to_kill_on_close_job`]) and `run_bounded`'s per-call [`Reaper`]:
/// which job to use is the caller's choice, the assignment itself is identical.
/// Takes ownership of neither handle.
#[cfg(target_os = "windows")]
fn assign_process_to_job(
    job: ::windows::Win32::Foundation::HANDLE,
    process: std::os::windows::io::RawHandle,
) -> bool {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::System::JobObjects::AssignProcessToJobObject;

    // SAFETY: `job` is a live job handle owned by the caller, and `process` is a
    // live child handle the caller keeps open; assignment borrows both.
    match unsafe { AssignProcessToJobObject(job, HANDLE(process)) } {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!("Failed to assign a child process to a kill-on-close job object: {e}");
            false
        }
    }
}

/// Owns the process-wide job handle. `HANDLE` is a raw pointer and so neither
/// `Send` nor `Sync`; a job handle is just a kernel object reference with no
/// thread affinity, so sharing it across threads is sound.
#[cfg(target_os = "windows")]
struct JobHandle(::windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
unsafe impl Send for JobHandle {}
#[cfg(target_os = "windows")]
unsafe impl Sync for JobHandle {}

/// The process-wide kill-on-close job, created on first use.
///
/// The handle is intentionally never closed. Its lifetime *is* the mechanism:
/// the job kills its members when the last handle to it closes, and we want
/// that to happen exactly when our process dies. Leaking it into a `OnceLock`
/// leaves the close to the kernel at process teardown, which is the one moment
/// that fires on every exit path including the ones that never run our code.
///
/// `None` if the job could not be set up; the caller then just loses the
/// backstop and keeps the graceful path.
#[cfg(target_os = "windows")]
fn kill_on_close_job() -> Option<::windows::Win32::Foundation::HANDLE> {
    static JOB: std::sync::OnceLock<Option<JobHandle>> = std::sync::OnceLock::new();

    // The singleton's handle is intentionally never closed (see the doc above);
    // leaking it into the OnceLock leaves the close to the kernel at teardown.
    JOB.get_or_init(create_kill_on_close_job)
        .as_ref()
        .map(|job| job.0)
}

/// Create a fresh kill-on-close job object, or `None` on failure. Shared by the
/// process-wide singleton above and the per-call jobs [`run_bounded`] uses to
/// reap a bounded child's descendants, so the `KILL_ON_JOB_CLOSE` limit has one
/// home. The caller owns the returned handle and decides when to close it (the
/// singleton never does; `run_bounded`'s [`Reaper`] does on drop).
#[cfg(target_os = "windows")]
fn create_kill_on_close_job() -> Option<JobHandle> {
    use ::windows::core::PCWSTR;
    use ::windows::Win32::Foundation::CloseHandle;
    use ::windows::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: Win32 job-object FFI. The unnamed job is created with default
    // security and is owned solely by the caller; on the error path we close it
    // before returning, so no handle leaks.
    unsafe {
        let job = match CreateJobObjectW(None, PCWSTR::null()) {
            Ok(job) => job,
            Err(e) => {
                tracing::warn!("Failed to create the kill-on-close job object: {e}");
                return None;
            }
        };

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) {
            tracing::warn!("Failed to set the kill-on-close limit on the job object: {e}");
            let _ = CloseHandle(job);
            return None;
        }

        Some(JobHandle(job))
    }
}

/// Deliver a graceful `CTRL_BREAK_EVENT` to a child process group on Windows.
///
/// Returns `true` if the event was delivered, `false` if it could not be (the
/// child already exited, or its console is unreachable) — the caller should
/// then fall back to `TerminateProcess`.
///
/// `pid` must be the PID of a child spawned with `CREATE_NEW_PROCESS_GROUP`
/// (see `configure_daemon_tokio_command`); for such a child the process-group
/// id equals its PID. `CTRL_BREAK_EVENT` is the only usable signal here:
/// `CREATE_NEW_PROCESS_GROUP` disables CTRL+C for the group, and unlike
/// `CTRL_C_EVENT` a break can target a specific group id.
///
/// The desktop app is a GUI process with no console, so a bare
/// `GenerateConsoleCtrlEvent` would have nothing to signal through. We
/// transiently attach to the child's (hidden) console, suppress the event in
/// ourselves so we don't self-terminate, broadcast it, then detach. This
/// mutates whole-process console state, so it is serialized under a lock; it
/// is also known to be finicky, hence the caller's `TerminateProcess`
/// fallback.
///
/// A release build is a GUI (windows-subsystem) process and owns no console,
/// so the detach is a no-op. A dev/console build run from a terminal (so the
/// daemon's tracing is visible) does own one; detaching it would tear that
/// terminal down, so we record it up front and reattach to it before
/// returning on every exit path.
#[cfg(target_os = "windows")]
pub fn send_ctrl_break(pid: u32) -> bool {
    use ::windows::Win32::Foundation::HANDLE;
    use ::windows::Win32::System::Console::{
        AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleWindow, GetStdHandle,
        SetConsoleCtrlHandler, SetStdHandle, ATTACH_PARENT_PROCESS, CTRL_BREAK_EVENT,
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };

    // Serialize: AttachConsole/FreeConsole/SetConsoleCtrlHandler mutate
    // per-process (not per-thread) console state, so two concurrent sends
    // would corrupt each other.
    static CONSOLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = CONSOLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // SAFETY: serialized Win32 console FFI. We restore the ctrl handler, the
    // standard handles, and our original console attachment before returning
    // regardless of outcome; no handle or console state escapes this function.
    unsafe {
        // Record whether we own a real console (one with a window) before we
        // touch any console state. A GUI release build owns none, so this is
        // false and the detach below is a no-op. A dev/console build run from
        // a terminal owns one; we reattach to it on the way out so a shutdown
        // attempt doesn't tear the terminal down.
        let had_console = !GetConsoleWindow().0.is_null();

        // Save our standard handles up front and restore them on every exit
        // path. AttachConsole/FreeConsole mutate whole-process console state
        // and leave this (GUI, console-less) process's STD_INPUT_HANDLE
        // dangling — NULL at launch, but an invalid non-NULL value once we
        // attach to and then free the child's console. Anything we spawn after
        // a shutdown attempt (notably the daemon respawn on restart) would then
        // inherit that invalid handle, and because the daemon command
        // redirects stdout/stderr (setting STARTF_USESTDHANDLES, which requires
        // all three standard handles to be valid) CreateProcess fails with
        // ERROR_INVALID_HANDLE. Restoring the saved values keeps our handle
        // state exactly as it was before the call. (The daemon command also
        // pins stdin to NUL as a belt-and-suspenders measure; this restore
        // protects any other post-shutdown spawn too.)
        //
        // GetStdHandle returns Err only for INVALID_HANDLE_VALUE; a console-
        // less process legitimately has NULL standard handles, which come back
        // as Ok(NULL). We coerce either case to a concrete HANDLE and restore
        // it unconditionally, so a process that started with NULL handles ends
        // with NULL handles rather than whatever the console churn left behind.
        let null_handle = HANDLE(std::ptr::null_mut());
        let saved_in = GetStdHandle(STD_INPUT_HANDLE).unwrap_or(null_handle);
        let saved_out = GetStdHandle(STD_OUTPUT_HANDLE).unwrap_or(null_handle);
        let saved_err = GetStdHandle(STD_ERROR_HANDLE).unwrap_or(null_handle);
        let restore = || {
            // Reattach to our original (parent's) console first for dev/console
            // builds; AttachConsole resets the standard handles, so the handle
            // restore must come after it.
            if had_console {
                let _ = AttachConsole(ATTACH_PARENT_PROCESS);
            }
            let _ = SetStdHandle(STD_INPUT_HANDLE, saved_in);
            let _ = SetStdHandle(STD_OUTPUT_HANDLE, saved_out);
            let _ = SetStdHandle(STD_ERROR_HANDLE, saved_err);
        };

        // Detach from any console we currently hold; otherwise AttachConsole
        // fails with ERROR_ACCESS_DENIED (a process can attach to at most one
        // console). Harmless if we have none.
        let _ = FreeConsole();
        if AttachConsole(pid).is_err() {
            // Child gone, or its console is not reachable.
            restore();
            return false;
        }
        // Make ourselves ignore the event we are about to broadcast so we
        // don't terminate the desktop along with the child. AttachConsole
        // resets the handler table, so this must come after it.
        let _ = SetConsoleCtrlHandler(None, true);
        let delivered = GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid).is_ok();
        let _ = SetConsoleCtrlHandler(None, false);
        let _ = FreeConsole();
        restore();
        delivered
    }
}

/// Test helper: collect a command's staged env edits into (set, removed).
/// `get_envs` yields `(key, None)` for a var marked for removal and
/// `(key, Some(v))` for one that is set. Module-level so [`super::pip`]'s
/// isolation tests share it.
#[cfg(test)]
pub(super) fn env_edits(cmd: &std::process::Command) -> (Vec<(String, String)>, Vec<String>) {
    let mut set = Vec::new();
    let mut removed = Vec::new();
    for (k, v) in cmd.get_envs() {
        let k = k.to_string_lossy().into_owned();
        match v {
            Some(v) => set.push((k, v.to_string_lossy().into_owned())),
            None => removed.push(k),
        }
    }
    (set, removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolate_python_command_disables_user_site() {
        let mut cmd = std::process::Command::new("python3");
        isolate_python_command(&mut cmd);
        let (set, _) = env_edits(&cmd);
        assert!(set.contains(&("PYTHONNOUSERSITE".to_string(), "1".to_string())));
    }

    #[test]
    fn isolate_python_command_strips_ambient_python_vars() {
        let mut cmd = std::process::Command::new("python3");
        isolate_python_command(&mut cmd);
        let (_, removed) = env_edits(&cmd);
        for var in ["PYTHONPATH", "PYTHONHOME", "PYTHONSTARTUP"] {
            assert!(removed.contains(&var.to_string()), "{var} not removed");
        }
    }

    /// The tokio variants reach through `as_std_mut`, which holds only because
    /// tokio's `Command` stages env on the very `std::process::Command` it later
    /// spawns. Assert the two variants stage identical env rather than
    /// re-listing the vars, so this fails if that ever stops being true. The
    /// std-side tests above prove the compared value isn't vacuously empty.
    #[test]
    fn isolate_python_tokio_command_matches_std_variant() {
        let mut std_cmd = std::process::Command::new("python3");
        isolate_python_command(&mut std_cmd);
        let mut tokio_cmd = tokio::process::Command::new("python3");
        isolate_python_tokio_command(&mut tokio_cmd);
        assert_eq!(env_edits(&std_cmd), env_edits(tokio_cmd.as_std()));
    }

    /// PID of `parent`'s child named `exe_name`, via a process snapshot.
    /// Used to reach the grandchild the test needs to assert on.
    ///
    /// Matching on the image name rather than taking the first child: a console
    /// is still allocated even under `CREATE_NO_WINDOW`, so `conhost.exe` can
    /// show up parented alongside the process we actually want.
    #[cfg(target_os = "windows")]
    fn child_pid_named(parent: u32, exe_name: &str) -> Option<u32> {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        };

        let mut found = None;

        // SAFETY: the snapshot handle is closed on every exit path, and
        // `entry` is initialized with the `dwSize` the API requires before
        // being handed to Process32FirstW/NextW.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return None;
            };
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut ok = Process32FirstW(snapshot, &mut entry).is_ok();
            while ok {
                if entry.th32ParentProcessID == parent {
                    // Slice at the first NUL rather than converting the whole
                    // fixed buffer and trimming: `entry` is reused every
                    // iteration and nothing promises the API zero-fills past
                    // the terminator, so a short name landing after a longer
                    // one leaves stale tail bytes ("ping.exe\0er.exe"). Those
                    // survive a trailing-NUL trim and silently fail the match.
                    let len = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
                    if name.eq_ignore_ascii_case(exe_name) {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                }
                ok = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }

        found
    }

    /// Resume a process spawned with `CREATE_SUSPENDED` by resuming its threads.
    ///
    /// `std::process::Command` hands back only a process handle, so reaching the
    /// initial thread means going through a thread snapshot.
    #[cfg(target_os = "windows")]
    fn resume_process(pid: u32) -> bool {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
        };
        use ::windows::Win32::System::Threading::{
            OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        };

        let mut resumed = false;

        // SAFETY: snapshot and thread handles are closed on every exit path;
        // `entry` carries the `dwSize` the API requires before first use.
        unsafe {
            let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) else {
                return false;
            };
            let mut entry = THREADENTRY32 {
                dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
                ..Default::default()
            };
            let mut ok = Thread32First(snapshot, &mut entry).is_ok();
            while ok {
                if entry.th32OwnerProcessID == pid {
                    if let Ok(thread) = OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID)
                    {
                        // ResumeThread returns the *previous* suspend count, or
                        // -1 on failure. Only a count of 1 or more is a real
                        // resume: 0 means the thread was already running, which
                        // is precisely the state that would make the caller's
                        // inheritance assertion vacuous rather than wrong, so it
                        // must not count.
                        match ResumeThread(thread) {
                            u32::MAX | 0 => {}
                            _ => resumed = true,
                        }
                        let _ = CloseHandle(thread);
                    }
                }
                ok = Thread32Next(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
        }

        resumed
    }

    /// Whether `pid` is a member of `job`, and a handle-terminate of it, in one
    /// pass so the test can both assert on a grandchild and clean it up.
    #[cfg(target_os = "windows")]
    fn grandchild_in_job_then_kill(
        pid: u32,
        job: ::windows::Win32::Foundation::HANDLE,
    ) -> Option<bool> {
        use ::windows::Win32::Foundation::CloseHandle;
        use ::windows::Win32::System::JobObjects::IsProcessInJob;
        use ::windows::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
        };

        // SAFETY: the opened handle is closed on every exit path; the BOOL out
        // param is a live local.
        unsafe {
            let handle = OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                false,
                pid,
            )
            .ok()?;
            let mut in_job = ::windows::core::BOOL(0);
            let queried = IsProcessInJob(handle, Some(job), &mut in_job).is_ok();
            let _ = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
            queried.then(|| in_job.as_bool())
        }
    }

    /// The wiring half: a real child lands in the job, a descendant of it
    /// inherits membership, and the job carries the limit flag that makes
    /// membership fatal. Without the flag the assignment would still "succeed"
    /// and buy us nothing; without the inheritance the compile-subtree and
    /// `git.exe` story is untrue.
    ///
    /// The other half — that membership actually kills when the owner dies — is
    /// `job_kills_its_member_when_the_owner_is_force_killed`.
    #[cfg(target_os = "windows")]
    #[test]
    fn daemon_child_lands_in_a_kill_on_close_job() {
        use ::windows::Win32::System::JobObjects::{
            IsProcessInJob, JobObjectExtendedLimitInformation, QueryInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use ::windows::Win32::System::Threading::{CREATE_NO_WINDOW, CREATE_SUSPENDED};
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;

        let job = kill_on_close_job().expect("job object should be creatable");

        // `cmd.exe` runs `ping` as a *grandchild*: only cmd is assigned to the
        // job, so ping is covered purely by the inheritance this asserts on.
        //
        // CREATE_SUSPENDED is what makes that assertion deterministic. Job
        // membership is only inherited by processes created *after* the parent
        // joins, so if cmd got to run `ping` before the assignment below, ping
        // would legitimately not be in the job and this would fail as a flake
        // that reads like a real inheritance bug. Starting cmd suspended means
        // it cannot spawn anything until we resume it, which is strictly after
        // the assignment. (The production spawn accepts this same race rather
        // than paying for it; see the note in `daemon::start_inner`.)
        //
        // CREATE_NO_WINDOW is folded in here rather than via
        // `configure_no_window_command` because `creation_flags` overwrites
        // rather than accumulates. Without it a local `cargo test` flashes a
        // console per run.
        let mut cmd = std::process::Command::new("cmd.exe");
        cmd.args(["/c", "ping", "-n", "30", "127.0.0.1"]);
        cmd.creation_flags((CREATE_NO_WINDOW | CREATE_SUSPENDED).0);
        let mut child = cmd.spawn().expect("failed to spawn test child");

        let assigned = assign_to_kill_on_close_job(child.as_raw_handle());
        let resumed = resume_process(child.id());

        // Poll rather than sleep a fixed guess, and don't hang the suite if
        // ping never appears.
        let mut grandchild = None;
        for _ in 0..100 {
            if let Some(pid) = child_pid_named(child.id(), "ping.exe") {
                grandchild = Some(pid);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // Kills the grandchild as it goes; `child.kill()` below reaps only cmd
        // itself, which would otherwise leave ping running for its full 30s.
        let grandchild_in_job = grandchild.and_then(|pid| grandchild_in_job_then_kill(pid, job));

        // SAFETY: both handles are live — `job` is the process-wide job and the
        // child handle is kept open by `child`, which outlives this call.
        let in_job = unsafe {
            let mut in_job = ::windows::core::BOOL(0);
            IsProcessInJob(
                ::windows::Win32::Foundation::HANDLE(child.as_raw_handle()),
                Some(job),
                &mut in_job,
            )
            .expect("IsProcessInJob failed");
            in_job.as_bool()
        };

        // SAFETY: `job` is a live job handle; the out buffer and its declared
        // length match `JobObjectExtendedLimitInformation`.
        let flags = unsafe {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            QueryInformationJobObject(
                Some(job),
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                None,
            )
            .expect("QueryInformationJobObject failed");
            info.BasicLimitInformation.LimitFlags
        };

        let _ = child.kill();
        let _ = child.wait();

        assert!(assigned, "child should have been assigned to the job");
        assert!(
            resumed,
            "suspended child was never resumed; the inheritance assertion below \
             would be vacuous rather than wrong"
        );
        assert!(in_job, "child should be a member of the job");
        assert_eq!(
            grandchild_in_job,
            Some(true),
            "a descendant of the assigned child must inherit job membership; the backend's \
             compilers and git children are only covered because of this"
        );
        assert!(
            flags.0 & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.0 != 0,
            "job must be kill-on-close, otherwise membership does not outlive-protect anything"
        );
    }

    /// Set on the re-executed test binary to put it in helper mode.
    #[cfg(target_os = "windows")]
    const JOB_HELPER_ENV: &str = "ESPHOME_JOB_KILL_HELPER";

    /// How the helper hands its member's PID back to the driver.
    #[cfg(target_os = "windows")]
    const JOB_HELPER_MARKER: &str = "JOB_MEMBER_PID=";

    #[cfg(target_os = "windows")]
    const JOB_HELPER_TEST: &str = "platform::process::tests::job_kill_helper";

    /// The owner half of `job_kills_its_member_when_the_owner_is_force_killed`.
    ///
    /// `#[ignore]` so a normal `cargo test` never runs it: it is only meaningful
    /// when the driver re-execs this binary with `--ignored --exact` and
    /// `JOB_HELPER_ENV` set, and it deliberately blocks until killed. The env
    /// guard means an `--ignored` run by hand exits immediately rather than
    /// hanging for two minutes.
    #[cfg(target_os = "windows")]
    #[test]
    #[ignore]
    fn job_kill_helper() {
        if std::env::var(JOB_HELPER_ENV).is_err() {
            return;
        }

        // Spawn the member exactly the way `daemon::start_inner` spawns the
        // backend — tokio's Command, `configure_daemon_tokio_command`, and the
        // handle from tokio's `raw_handle()` — so this exercises the real code
        // path rather than a std::process lookalike that happens to agree.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("helper: tokio runtime");
        let child_pid = rt.block_on(async {
            let mut cmd = tokio::process::Command::new("ping.exe");
            cmd.args(["-n", "120", "127.0.0.1"]);
            configure_daemon_tokio_command(&mut cmd);
            let child = cmd.spawn().expect("helper: failed to spawn member");
            let handle = child.raw_handle().expect("helper: member has no handle");
            assert!(
                assign_to_kill_on_close_job(handle),
                "helper: could not assign the member to the job"
            );
            let pid = child.id().expect("helper: member has no pid");
            // Leak the Child: dropping it would let tokio reap or kill the
            // member, and the driver needs it alive until *we* are killed.
            std::mem::forget(child);
            pid
        });

        // Printing the PID is also the driver's signal that assignment is done,
        // so it can't kill us before the member is actually in the job.
        println!("{JOB_HELPER_MARKER}{child_pid}");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        // Block until the driver force-kills us. Bounded so a driver that dies
        // early leaves this to time out rather than wedge CI forever.
        std::thread::sleep(std::time::Duration::from_secs(120));
    }

    /// The claim the whole change rests on: when the owning process dies without
    /// running any of its own code, Windows kills the job's members.
    ///
    /// Everything else about this feature is verifiable in-process, but not this
    /// — the owner has to actually die, and it can't be the test process. So the
    /// test binary re-execs itself as the owner, waits for it to report a member
    /// it has assigned, then `TerminateProcess`es it. That is precisely the
    /// shape of the cases this exists for: the NSIS uninstaller force-killing
    /// us, a crash, End Task. No `Drop`, no exit handler, no cooperation.
    ///
    /// A handle to the member is opened *before* the kill, so the PID can't be
    /// recycled underneath us and the wait is on the member itself rather than a
    /// poll for its absence.
    #[cfg(target_os = "windows")]
    #[test]
    fn job_kills_its_member_when_the_owner_is_force_killed() {
        use ::windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use ::windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        use std::io::{BufRead, BufReader};

        let exe = std::env::current_exe().expect("current_exe");
        let mut owner = std::process::Command::new(exe)
            .args(["--ignored", "--exact", JOB_HELPER_TEST, "--nocapture"])
            .env(JOB_HELPER_ENV, "1")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn the owner helper");

        let stdout = owner.stdout.take().expect("owner stdout");
        let member_pid = BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .find_map(|line| {
                line.strip_prefix(JOB_HELPER_MARKER)
                    .and_then(|pid| pid.trim().parse::<u32>().ok())
            });
        let Some(member_pid) = member_pid else {
            let _ = owner.kill();
            panic!("the owner never reported a job member; it likely failed to assign one");
        };

        // SAFETY: `member_pid` was just reported by a live child; the handle is
        // closed on every path below.
        let member = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, member_pid) }
            .expect("could not open the job member");

        // The point of the whole test: kill the owner outright.
        owner.kill().expect("failed to kill the owner");
        let _ = owner.wait();

        // SAFETY: `member` is a live handle we own and close immediately after.
        let waited = unsafe { WaitForSingleObject(member, 15_000) };
        let _ = unsafe { CloseHandle(member) };

        assert_eq!(
            waited, WAIT_OBJECT_0,
            "the job did not kill its member when the owning process was force-killed; \
             the backend would survive the desktop and keep holding its trees open"
        );
    }

    #[test]
    fn tail_for_log_passes_short_input_through_trimmed() {
        assert_eq!(tail_for_log("  hello  "), "hello");
        assert_eq!(tail_for_log("plain"), "plain");
    }

    #[test]
    fn tail_for_log_keeps_input_at_exactly_the_limit() {
        let s = "a".repeat(PIP_STDERR_TAIL_BYTES);
        let out = tail_for_log(&s);
        assert_eq!(out, s, "input exactly at the limit must pass through");
        assert!(!out.contains("truncated"), "no marker at the boundary");
    }

    #[test]
    fn tail_for_log_truncates_to_the_tail_with_marker() {
        let s = "x".repeat(PIP_STDERR_TAIL_BYTES + 904);
        let out = tail_for_log(&s);
        assert!(
            out.starts_with("...(stderr truncated"),
            "marker comes first"
        );
        assert!(
            out.ends_with(&s[s.len() - PIP_STDERR_TAIL_BYTES..]),
            "keeps tail"
        );
    }

    #[test]
    fn tail_for_log_does_not_split_a_multibyte_char() {
        // 1366 * 3 bytes = 4098 > 4096; the naive cut at len-4096 lands at
        // byte 2, mid-"€". The function advances past the partial leading
        // char to the next char boundary, so the result stays valid UTF-8
        // and never panics.
        let s = "€".repeat(1366);
        let out = tail_for_log(&s);
        assert!(out.contains("truncated"), "long input must be marked");
        let tail = out.split_once('\n').unwrap().1;
        assert!(
            tail.len() <= PIP_STDERR_TAIL_BYTES,
            "tail stays within bound"
        );
        assert!(tail.chars().all(|c| c == '€'), "no partial char survives");
    }

    #[test]
    fn run_python_capture_bounded_kills_a_child_that_will_not_exit() {
        // The probe runs in front of daemon.start(); an unbounded child there
        // means the backend never starts and nothing says why.
        let python = Path::new(TEST_PYTHON);
        let started = std::time::Instant::now();
        let err = run_python_capture_bounded(
            python,
            ["-c", "import time; time.sleep(600)"],
            std::time::Duration::from_millis(300),
        )
        .expect_err("a sleeping child must hit the deadline");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut, "{err}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline did not fire promptly: {:?}",
            started.elapsed()
        );
    }

    /// Extract the grandchild pid the test child reports as `GRANDCHILD_PID=<n>`,
    /// tolerating surrounding text (the timeout error wraps it in a message).
    fn parse_grandchild_pid(bytes: &[u8]) -> Option<u32> {
        let s = String::from_utf8_lossy(bytes);
        let (_, after) = s.split_once("GRANDCHILD_PID=")?;
        after
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    }

    /// Assert `pid` (a grandchild the bounded call should have reaped) is dead.
    ///
    /// Unix: a SIGKILL'd grandchild is reaped by init asynchronously, so poll --
    /// signal 0 probes existence and ESRCH means gone. pid reuse in the poll
    /// window is not a concern here (sequential, slow-cycling pids).
    #[cfg(not(target_os = "windows"))]
    fn assert_pid_reaped(pid: u32) {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !matches!(
            kill(Pid::from_raw(pid as i32), None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "grandchild pid {pid} survived the bounded call; the tree was not reaped"
            );
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Windows: open ONE handle and wait on it rather than re-opening by pid
    /// each poll -- the handle pins this exact process object, so a pid recycled
    /// on a busy runner can't make a dead grandchild look alive. A failed open
    /// means the process is already gone.
    #[cfg(target_os = "windows")]
    fn assert_pid_reaped(pid: u32) {
        use ::windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use ::windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: OpenProcess by pid; the handle is closed on every path.
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
                return;
            };
            let waited = WaitForSingleObject(handle, 10_000);
            let _ = CloseHandle(handle);
            assert_eq!(
                waited, WAIT_OBJECT_0,
                "grandchild pid {pid} survived the bounded call; the tree was not reaped"
            );
        }
    }

    #[test]
    fn run_bounded_reaps_a_grandchild_after_the_child_exits() {
        // pip on the SUCCESS path spawns PEP 517 build backends that inherit its
        // pipes and can outlive it. The child here models that: it writes to
        // stderr, spawns a grandchild that holds the pipe open and sleeps far
        // longer than the test tolerates, reports the grandchild pid, then exits
        // 0. The deadline must not stall (a grandchild on the pipe is why the
        // reader wait is bounded), the child's partial stderr must survive, and
        // -- the point of #344 -- the grandchild must be dead afterwards.
        let started = std::time::Instant::now();
        let out = run_python_capture_bounded(
            Path::new(TEST_PYTHON),
            [
                "-c",
                "import subprocess,sys; sys.stderr.write('before\\n'); sys.stderr.flush(); \
                 p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)']); \
                 sys.stdout.write('GRANDCHILD_PID=%d\\n'%p.pid); sys.stdout.flush(); \
                 sys.exit(0)",
            ],
            std::time::Duration::from_secs(60),
        )
        .expect("the child exited, so this must return");

        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "a grandchild holding the pipe stalled the call for {:?}",
            started.elapsed()
        );
        assert!(out.status.success());
        // What the child managed to write before the grandchild pinned the pipe
        // still comes back.
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("before"),
            "gave up on the reader without keeping what it had already read"
        );
        let pid = parse_grandchild_pid(&out.stdout)
            .expect("the child must report its grandchild's pid on stdout");
        assert_pid_reaped(pid);
    }

    #[test]
    fn run_bounded_reaps_a_grandchild_on_timeout() {
        // The child never exits (sleeps well past the bound) and leaves a
        // grandchild that would too; the bound must fire AND leave neither
        // behind. The grandchild pid goes to stderr, which the timed-out error
        // carries in its message.
        let out = run_python_capture_bounded(
            Path::new(TEST_PYTHON),
            [
                "-c",
                "import subprocess,sys,time; \
                 p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(600)']); \
                 sys.stderr.write('GRANDCHILD_PID=%d\\n'%p.pid); sys.stderr.flush(); \
                 time.sleep(600)",
            ],
            std::time::Duration::from_secs(2),
        );
        let err = out.expect_err("the child never exits, so this must time out");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        let pid = parse_grandchild_pid(err.to_string().as_bytes())
            .expect("the timed-out child must have reported its grandchild's pid");
        assert_pid_reaped(pid);
    }

    #[test]
    fn run_python_capture_bounded_returns_output_within_the_deadline() {
        let out = run_python_capture_bounded(
            Path::new(TEST_PYTHON),
            ["-c", "print('hi')"],
            std::time::Duration::from_secs(60),
        )
        .expect("a trivial script must not time out");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    }

    /// Any interpreter will do for the bounded-capture tests: they exercise the
    /// process plumbing, not the bundled tree. Named rather than probed for, so a
    /// host without it fails these tests loudly instead of skipping them — a
    /// timeout test that quietly reports green is worse than no timeout test.
    /// Every platform we build on has `python3` (the Python jobs install it, and
    /// `prepare_bundle.sh` needs one regardless).
    #[cfg(not(target_os = "windows"))]
    const TEST_PYTHON: &str = "python3";

    #[cfg(target_os = "windows")]
    const TEST_PYTHON: &str = "python";
}
