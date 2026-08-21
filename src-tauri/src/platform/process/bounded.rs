//! Running a child process under a deadline: draining its pipes off-thread so
//! it cannot block on a full one, and reaping its whole process tree so a
//! descendant that inherited a pipe cannot outlive the bound.
//!
//! Policy-free by design — which interpreter, which isolation, which pipes, and
//! what any of the outcomes mean all stay with the caller in [`super`].

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
pub(in crate::platform) enum BoundedRun {
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
pub(in crate::platform) fn run_bounded(
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
    // signalled on the bound (matches `daemon::spawn::start_inner`). Windows: nothing
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
    job: Option<super::JobHandle>,
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
        let job = super::create_kill_on_close_job();
        // Accept the small CreateProcess->assign race (a descendant spawned in
        // that window escapes the job), exactly as `daemon::spawn::start_inner` does:
        // closing it needs CREATE_SUSPENDED + a thread handle std does not
        // expose, and the callers spawn Python, which spawns nothing that early.
        // `super::assign_process_to_job` / `super::create_kill_on_close_job` log
        // the Win32 cause; this logs the consequence.
        let covered = job
            .as_ref()
            .is_some_and(|j| super::assign_process_to_job(j.0, child.as_raw_handle()));
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

/// A pipe reader task, and the bytes it has accumulated so far — the async
/// counterpart to [`run_bounded`]'s reader threads, for callers that bound a
/// [`tokio::process::Child`] with `tokio::time::timeout` instead.
///
/// Same reasons, same shape: a child whose output fills a pipe buffer (~64 KiB)
/// blocks on `write` until someone reads the other end, so the pipes must be
/// drained off the caller's task or the child outlives the very deadline meant
/// to bound it; and the bytes are accumulated where the caller can reach them
/// without the reader's cooperation, so a reader still blocked on a pipe a
/// surviving grandchild holds open cannot keep them hostage.
pub(in crate::platform) struct PipeDrain {
    /// Which stream this is, for the log lines. The only thing in here that is
    /// not policy-free.
    what: &'static str,
    /// Shared rather than returned from the task, so the bytes are readable
    /// while the reader is still running — which on a timeout path is the only
    /// time they are readable at all.
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

/// Read a child pipe to EOF into a shared buffer, off the caller's task.
pub(in crate::platform) fn drain_pipe<R>(what: &'static str, handle: Option<R>) -> PipeDrain
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let task = handle.map(|mut h| {
        let task_buf = std::sync::Arc::clone(&buf);
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut chunk = [0u8; 8192];
            loop {
                match h.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => task_buf
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend_from_slice(&chunk[..n]),
                    // Keep what arrived before the failure and say the read
                    // broke, rather than reporting a short buffer as if the
                    // child had printed nothing.
                    Err(e) => {
                        tracing::warn!("Lost part of a child's {what}: {e}");
                        break;
                    }
                }
            }
        })
    });
    PipeDrain { what, buf, task }
}

impl PipeDrain {
    /// Wait for the reader to finish, for at most [`DRAIN_GRACE`], then give up
    /// on it and take what it has.
    ///
    /// Giving up *aborts* the reader rather than dropping it: dropping a
    /// `JoinHandle` detaches its task, so a reader blocked on a pipe a
    /// grandchild still holds open would stay resident for the life of the
    /// process, holding the fd and appending to a buffer nobody will read
    /// again. Cancellation lands at the `read` await, never while the buffer
    /// lock is held, so the bytes already accumulated are unaffected.
    ///
    /// A panicking reader costs only the tail of the output: it panics inside
    /// `extend_from_slice`, and the poison recovery below hands back the bytes
    /// it had already read.
    pub(in crate::platform) async fn collect(mut self) -> Vec<u8> {
        let what = self.what;
        if let Some(task) = self.task.as_mut() {
            match tokio::time::timeout(DRAIN_GRACE, &mut *task).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => tracing::warn!(
                    "The reader for a child's {what} panicked; returning what arrived."
                ),
                Err(_) => {
                    tracing::warn!(
                        "A child's {what} reader did not finish within {DRAIN_GRACE:?}; \
                         something that inherited the pipe is still holding it open. \
                         Returning what arrived."
                    );
                    task.abort();
                }
            }
        }
        self.take()
    }

    /// Take the bytes read so far and stop the reader without waiting for it —
    /// for the deadline path, where the child is being abandoned and there is
    /// nothing left for the reader to contribute.
    pub(in crate::platform) fn abandon(mut self) -> Vec<u8> {
        let taken = self.take();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        taken
    }

    /// Take the bytes read so far, recovering a poisoned lock rather than
    /// unwrapping it — a `Vec<u8>` has no invariant for the poison to protect,
    /// and panicking here would throw away output the caller is about to
    /// report.
    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut *self.buf.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`PipeDrain::collect`] giving up on a reader must *end* it. Dropping a
    /// `JoinHandle` detaches its task, so the reader this grace exists for — one
    /// blocked on a pipe a surviving grandchild still holds open — would
    /// otherwise stay resident for the life of the process, holding the fd and
    /// appending to a buffer `collect` has already emptied and nobody will read
    /// again.
    ///
    /// Asserted on that buffer rather than on the handle: the grandchild keeps
    /// writing past the grace, so a detached reader grows it and an aborted one
    /// leaves it empty. Unix-only for the shell; the behaviour under test
    /// (`JoinHandle::abort`) is tokio's and not platform-specific.
    #[cfg(unix)]
    #[tokio::test]
    async fn collect_aborts_a_reader_it_gave_up_on() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg("echo first; sh -c 'while :; do echo late; sleep 0.2; done' & exit 0")
            .stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        let drain = drain_pipe("stdout", child.stdout.take());
        let buf = std::sync::Arc::clone(&drain.buf);
        child.wait().await.unwrap();

        // The grandchild holds the pipe open, so this returns on DRAIN_GRACE
        // rather than on EOF — the branch under test.
        let out = drain.collect().await;
        assert!(
            String::from_utf8_lossy(&out).contains("first"),
            "the output the child wrote before exiting was lost: {out:?}"
        );

        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let after = buf.lock().unwrap().clone();
        assert!(
            after.is_empty(),
            "the reader was detached rather than aborted and is still appending: {}",
            String::from_utf8_lossy(&after)
        );
    }
}
