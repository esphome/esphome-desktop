//! Everything pip-specific about driving the bundled interpreter: the install
//! command builders, the env isolation that keeps an install inside the
//! managed tree, the bounded blocking install the startup restore uses, and
//! the job-object-aware runner the update flows use. The generic child-process
//! machinery these compose (bounded execution, capture, window suppression)
//! lives in [`super::process`].

use anyhow::{Context, Result};
use std::path::Path;

use super::process::{
    configure_no_window_tokio_command, head_for_log, isolate_python_command, python_command,
    run_bounded, tail_for_log, BoundedRun,
};

/// Hard upper bound on a single `pip install` invocation during the
/// version-restore path. Five minutes is well over the time needed to upgrade
/// `esphome` on a working connection; bounding it prevents a stalled network
/// from hanging app startup indefinitely.
const PIP_INSTALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Synchronously run `pip install <package>==<version>` with a wall-clock
/// timeout. Pinning the exact version lets pip resolve pre-releases without
/// needing `--pre`. On timeout the child is killed and an error is returned;
/// the caller logs a warning and falls back to the bundled version, so a
/// stalled pip can't block app launch.
pub(super) fn pip_install_blocking(python_bin: &Path, package: &str, version: &str) -> Result<()> {
    let spec = format!("{}=={}", package, version);
    let mut cmd = python_command(python_bin, ["-m", "pip", "install", &spec]);
    // The builder isolates the interpreter; pip needs its own env off too, and
    // every edit is an idempotent `env`/`env_remove`, so layering is a no-op.
    isolate_pip_command(&mut cmd);
    // Both streams: pip logs a resolution failure's headline at CRITICAL
    // (stderr) but the block explaining WHICH requirements conflict at INFO
    // (stdout). This is a GUI process, so inherited stdout goes nowhere, and
    // stderr alone reports the symptom with none of the cause (#339).
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    match run_bounded(cmd, PIP_INSTALL_TIMEOUT).context("Failed to run pip install")? {
        BoundedRun::Exited(output) if output.status.success() => Ok(()),
        BoundedRun::Exited(output) => {
            anyhow::bail!(
                "pip install {} failed: {}",
                spec,
                pip_output_report(&output)
            )
        }
        BoundedRun::TimedOut { stderr } => anyhow::bail!(
            "pip install {} timed out after {:?}; partial stderr: {}",
            spec,
            PIP_INSTALL_TIMEOUT,
            tail_for_log(&String::from_utf8_lossy(&stderr))
        ),
    }
}

/// Start of the block in which pip explains a resolution failure.
const PIP_CONFLICT_MARKER: &str = "The conflict is caused by:";

/// Build the reported text for a failed pip install from its two streams.
///
/// pip splits a resolution failure across both. stderr carries the headline
/// alone — `ERROR: Cannot install esphome and esphome==2026.7.0 because these
/// package versions have conflicting dependencies` — because that line is the
/// only part logged at CRITICAL. The block naming *which* requirements
/// conflict, and whether one has no distribution for this environment at all,
/// is logged at INFO and therefore goes to stdout. Reporting stderr alone left
/// a bug report holding the symptom and none of the cause (#327, #339).
///
/// Everything from the marker onwards is that diagnostic, so append that and
/// nothing else: earlier stdout is `Collecting`/`Downloading` progress that
/// would bury it. A failure pip words differently (a build error, no network)
/// has no marker and reports stderr alone.
///
/// Each half is bounded on its own, because their interesting ends differ:
/// stderr's actionable line is last (tail), while the diagnostic opens with
/// the conflicting requirements and trails off into generic advice (head).
/// Bounding the joined report instead would let a long diagnostic evict the
/// headline and its own opening — the symptom and the cause — keeping only
/// the boilerplate.
fn pip_failure_report(stdout: &str, stderr: &str) -> String {
    let stderr = tail_for_log(stderr);
    match stdout.find(PIP_CONFLICT_MARKER) {
        Some(start) => format!("{stderr}\n{}", head_for_log(&stdout[start..])),
        None => stderr,
    }
}

/// [`pip_failure_report`] over a captured pip [`std::process::Output`].
/// Already bounded, so callers put it in an error or a log line as is.
/// `pub` because the update flows report their pip failures through the same
/// extraction (see `install_with_record_recovery`).
pub fn pip_output_report(output: &std::process::Output) -> String {
    pip_failure_report(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// pip settings that would send an install somewhere other than the managed
/// tree [`super::process::PYTHON_ISOLATION_SET`] just pinned the interpreter
/// to.
///
/// Both are load-bearing rather than theoretical. `user` is a common `sudo pip`
/// workaround, and it only ever "worked" here because the install went to user
/// site and user site was importable; with the latter now off, pip aborts the
/// install outright. `require-virtualenv` fails every pip call we make
/// regardless of user site, since the bundled tree is not a venv.
///
/// These are forced to `0` rather than unset because pip resolves config as
/// command line > env > config file. Unsetting only clears the ambient env var
/// and leaves a `user = true` in `~/.config/pip/pip.conf` in force; an explicit
/// `0` overrides the file too. Note this deliberately does not touch
/// `PIP_CONFIG_FILE`: dropping it would discard the rest of the user's pip
/// config (a corporate `index-url`, proxy settings) while still leaving the
/// default config files to be read, so it neutralizes nothing on its own.
const PIP_ISOLATION_SET: [(&str, &str); 2] = [("PIP_USER", "0"), ("PIP_REQUIRE_VIRTUALENV", "0")];

/// pip settings that repoint the install directly. Unlike
/// [`PIP_ISOLATION_SET`], these have no "off" value to force: pip strips empty
/// config values before it applies the override order (`if v` in
/// `ConfigOptionParser._get_ordered_configuration_items`), so `PIP_TARGET=""`
/// never reaches the defaults and the config file wins by fallthrough. Dropping
/// the ambient var is all that is available.
///
/// Known residual gap, deliberately not closed: this only clears the env var,
/// so a `target`/`prefix` in the user's own pip.conf still redirects the
/// install off the managed tree, and an ESPHome update then reports success
/// while landing somewhere this interpreter will never import. The only lever
/// that would neutralize it is pointing `PIP_CONFIG_FILE` at the platform's
/// null device (`/dev/null`, `NUL` on Windows), which throws away the rest of
/// their pip config (see [`PIP_ISOLATION_SET`]); that trade is not worth it for
/// a config this rare, and the gap predates the isolation work. Note pip's docs
/// spell that lever `os.devnull`, meaning the *value* of Python's constant: set
/// literally, it is just a relative path that does not exist, and pip silently
/// falls back to the default config files rather than erroring.
const PIP_ISOLATION_REMOVE: [&str; 2] = ["PIP_TARGET", "PIP_PREFIX"];

/// [`isolate_python_command`] plus the `PIP_*` config that would redirect the
/// install target. For commands running `-m pip`.
pub fn isolate_pip_command(cmd: &mut std::process::Command) {
    isolate_python_command(cmd);
    for (k, v) in PIP_ISOLATION_SET {
        cmd.env(k, v);
    }
    for k in PIP_ISOLATION_REMOVE {
        cmd.env_remove(k);
    }
}

/// [`isolate_pip_command`] for a tokio::process::Command. See
/// [`super::process::isolate_python_tokio_command`] on why editing the wrapped
/// command works.
pub fn isolate_pip_tokio_command(cmd: &mut tokio::process::Command) {
    isolate_pip_command(cmd.as_std_mut());
}

/// Build a tokio `pip install` command for the given Python interpreter,
/// prefilled with `-m pip install` and the Windows no-window flag, and
/// isolated from the ambient Python/pip environment so the install lands in
/// the managed tree (see [`isolate_pip_command`]). Callers append their own
/// package specs and flags before running it.
pub fn pip_command(python: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(python);
    cmd.args(["-m", "pip", "install"]);
    isolate_pip_tokio_command(&mut cmd);
    configure_no_window_tokio_command(&mut cmd);
    cmd
}

/// Run a prepared bundled-interpreter command (e.g. from [`pip_command`]) to
/// completion, capturing its output and — on Windows — tying the child to the
/// desktop's lifetime via the kill-on-close job. Without this, an install-dir
/// `python.exe` spawned during an update or channel switch is orphaned if the
/// desktop is force-killed mid-run, holding the install tree open and leaving
/// `site-packages` half-written (issue #333, the #320 failure by a route #320
/// does not cover).
///
/// Replicates [`tokio::process::Command::output`]'s capture (stdin closed,
/// stdout/stderr piped) rather than calling it, because the child must be
/// spawned before it can be assigned to the job. Callers parse both streams, so
/// they must not be inherited.
///
/// Best-effort assignment, exactly like the backend spawn in
/// [`crate::daemon`]: the graceful `CTRL_BREAK`/`TerminateProcess` paths still
/// apply, so a failed assignment warns and carries on rather than failing the
/// install. Job membership is a per-child policy, which is why this is a named
/// seam the pip sites opt into rather than something every spawn inherits.
pub async fn run_pip(mut cmd: tokio::process::Command) -> std::io::Result<std::process::Output> {
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd.spawn()?;

    #[cfg(windows)]
    if !child
        .raw_handle()
        .is_some_and(super::process::assign_to_kill_on_close_job)
    {
        tracing::warn!(
            "pip is not covered by the kill-on-close job; it may outlive the \
             desktop if this process is killed without running its shutdown path"
        );
    }

    child.wait_with_output().await
}

#[cfg(test)]
mod tests {
    use super::super::process::{
        env_edits, LOG_TAIL_BYTES, PYTHON_ISOLATION_REMOVE, PYTHON_ISOLATION_SET,
    };
    use super::*;

    /// pip resolves and installs against the same interpreter the backend runs
    /// on, so it has to see the same `sys.path` the backend will, and it must
    /// not be redirected off that tree by ambient `PIP_*` config.
    #[test]
    fn pip_command_is_isolated() {
        let cmd = pip_command(Path::new("python3"));
        let (set, removed) = env_edits(cmd.as_std());
        assert!(set.contains(&("PYTHONNOUSERSITE".to_string(), "1".to_string())));
        assert!(set.contains(&("PIP_USER".to_string(), "0".to_string())));
        assert!(removed.contains(&"PYTHONPATH".to_string()));
    }

    /// run_pip must reproduce `Command::output`'s capture. `pip_command` sets no
    /// stdio, so a bare `spawn().wait_with_output()` would inherit the streams
    /// and return them empty, silently blanking the output the update callers
    /// parse for RECORD recovery and error tails. A plain shell command (not
    /// `pip_command`) exercises the wrapper's spawn/capture directly.
    #[tokio::test]
    async fn run_pip_captures_stdout_and_stderr() {
        let (program, args) = if cfg!(windows) {
            ("cmd", ["/c", "echo out& echo err 1>&2& exit 1"])
        } else {
            ("sh", ["-c", "echo out; echo err 1>&2; exit 1"])
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);

        let output = run_pip(cmd).await.expect("run_pip should spawn and wait");
        assert!(
            !output.status.success(),
            "the non-zero exit must be reported"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("out"),
            "stdout was not captured"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("err"),
            "stderr was not captured"
        );
    }

    /// pip isolation is a superset of Python isolation: a pip install that
    /// lands outside the managed tree is as broken as an import that resolves
    /// outside it.
    #[test]
    fn isolate_pip_command_covers_python_isolation_too() {
        let mut cmd = std::process::Command::new("python3");
        isolate_pip_command(&mut cmd);
        let (set, removed) = env_edits(&cmd);
        for (k, v) in PYTHON_ISOLATION_SET.iter().chain(&PIP_ISOLATION_SET) {
            assert!(
                set.contains(&(k.to_string(), v.to_string())),
                "{k} not set to {v}"
            );
        }
        for var in PYTHON_ISOLATION_REMOVE.iter().chain(&PIP_ISOLATION_REMOVE) {
            assert!(removed.contains(&var.to_string()), "{var} not removed");
        }
    }

    /// See `isolate_python_tokio_command_matches_std_variant` in
    /// [`super::super::process`]'s tests: the tokio variant must stage exactly
    /// the env the std variant does.
    #[test]
    fn isolate_pip_tokio_command_matches_std_variant() {
        let mut std_cmd = std::process::Command::new("python3");
        isolate_pip_command(&mut std_cmd);
        let mut tokio_cmd = tokio::process::Command::new("python3");
        isolate_pip_tokio_command(&mut tokio_cmd);
        assert_eq!(env_edits(&std_cmd), env_edits(tokio_cmd.as_std()));
    }

    /// pip's precedence is command line > env > config file, so forcing `0`
    /// beats unsetting: a `user = true` in the user's pip.conf survives the
    /// latter. Pin the values, not just the keys, so a future edit back to
    /// `env_remove` fails here rather than in the field.
    #[test]
    fn pip_isolation_forces_off_rather_than_unsetting() {
        let mut cmd = std::process::Command::new("python3");
        isolate_pip_command(&mut cmd);
        let (set, removed) = env_edits(&cmd);
        for var in ["PIP_USER", "PIP_REQUIRE_VIRTUALENV"] {
            assert!(
                set.contains(&(var.to_string(), "0".to_string())),
                "{var} must be forced to 0, not unset"
            );
            assert!(!removed.contains(&var.to_string()));
        }
    }

    /// pip stdout for the #327 failure: progress noise, then the diagnostic.
    const CONFLICT_STDOUT: &str = "Collecting esphome==2026.7.0\n  Using cached esphome-2026.7.0-py3-none-any.whl\nINFO: pip is looking at multiple versions of esphome\n\nThe conflict is caused by:\n    The user requested esphome==2026.7.0\n    esphome 2026.7.0 depends on some-dep>=2\n\nAdditionally, some packages in these conflicts have no matching distributions available for your environment:\n    some-dep\n\nTo fix this you could try to:\n1. loosen the range of package versions you've specified\n";

    /// pip stderr for the same failure: the headline, and nothing else.
    const CONFLICT_STDERR: &str = "ERROR: Cannot install esphome and esphome==2026.7.0 because these package versions have conflicting dependencies.\nERROR: ResolutionImpossible: for help visit https://pip.pypa.io/en/latest/topics/dependency-resolution/\n";

    #[test]
    fn pip_failure_report_keeps_the_conflict_diagnostic() {
        // #327: stderr alone says two requirements conflict but never which,
        // so the report must carry stdout's block — including the line naming
        // the dependency with no distribution for this environment, which is
        // the whole reason the pinned install cannot resolve.
        let report = pip_failure_report(CONFLICT_STDOUT, CONFLICT_STDERR);
        assert!(report.contains("ERROR: Cannot install esphome and esphome==2026.7.0"));
        assert!(report.contains("The conflict is caused by:"));
        assert!(report.contains("esphome 2026.7.0 depends on some-dep>=2"));
        assert!(report.contains("no matching distributions available for your environment"));
    }

    #[test]
    fn pip_failure_report_drops_progress_noise_before_the_marker() {
        // Only the diagnostic tail is wanted; the Collecting/Downloading
        // chatter ahead of it would bury the cause in the dialog and the log.
        let report = pip_failure_report(CONFLICT_STDOUT, CONFLICT_STDERR);
        assert!(!report.contains("Collecting esphome==2026.7.0"));
        assert!(!report.contains("Using cached"));
    }

    #[test]
    fn pip_failure_report_bounds_a_huge_diagnostic_without_losing_its_start() {
        // A real resolution failure can push the conflict block far past the
        // log cap. Tailing the joined report would evict the stderr headline
        // and the block's opening — the packages that actually conflict —
        // keeping only pip's trailing boilerplate advice. Each half is
        // bounded on its own instead, so both survive.
        let huge_stdout = format!(
            "Collecting esphome==2026.7.0\nThe conflict is caused by:\n    \
             esphome 2026.7.0 depends on some-dep>=2\n{}",
            "x".repeat(LOG_TAIL_BYTES * 3)
        );
        let report = pip_failure_report(&huge_stdout, CONFLICT_STDERR);
        assert!(report.contains("ERROR: Cannot install esphome and esphome==2026.7.0"));
        assert!(report.contains("The conflict is caused by:"));
        assert!(report.contains("esphome 2026.7.0 depends on some-dep>=2"));
        assert!(report.contains("...(truncated to first"));
        assert!(
            report.len() <= 2 * LOG_TAIL_BYTES + 100,
            "report is bounded"
        );
    }

    #[test]
    fn pip_failure_report_without_a_marker_is_stderr_alone() {
        // A failure pip words differently (a build error, no network) has no
        // marker, and must report exactly what it did before.
        let report = pip_failure_report(
            "Collecting esphome==2026.7.0\n",
            "ERROR: Could not find a version that satisfies the requirement esphome==2026.7.0\n",
        );
        assert_eq!(
            report,
            "ERROR: Could not find a version that satisfies the requirement esphome==2026.7.0"
        );
    }

    /// The wiring the pure tests above cannot see: both streams are actually
    /// piped and drained into the report. A stub pip prints the resolution
    /// story the way the real one splits it (headline on stderr, marker block
    /// on stdout) and exits 1; un-piping stdout again would reproduce #339,
    /// an error holding the symptom and none of the cause.
    #[cfg(unix)]
    #[test]
    fn pip_install_blocking_failure_carries_the_stdout_diagnostic() {
        let base = crate::util::unique_temp_dir("pip-blocking-diagnostic");
        let bin = crate::platform::python_env::write_stub_interpreter(
            &base,
            "echo 'The conflict is caused by:'\n\
             echo 'ERROR: ResolutionImpossible' >&2\n\
             exit 1",
        );

        let err = pip_install_blocking(&bin, "esphome", "2026.7.0").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ERROR: ResolutionImpossible"), "{msg}");
        assert!(msg.contains("The conflict is caused by:"), "{msg}");
        let _ = std::fs::remove_dir_all(&base);
    }
}
