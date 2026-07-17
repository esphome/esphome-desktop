#!/usr/bin/env python3
"""Determine which CI jobs to run for a pull request from its changed files.

Emits JSON to stdout, e.g. ``{"build": true, "lint_test": true}``. Both
``build.yml`` and ``lint-test.yml`` read this to skip their expensive jobs on a
pull request that only touches documentation or repo meta files, so a docs-only
PR no longer spends ~60 minutes in the Tauri build matrix. The four required
``Build *`` checks are replaced in branch protection by a single ``CI Status``
aggregate that treats a skipped build as a pass, so skipping never blocks the
merge.

The bias is one-directional on purpose: a job is skipped only when *every*
changed file is provably CI-irrelevant (the conservative allowlist below), and
any error determining the changed set falls back to running everything. A real
code change can therefore never be skipped by mistake; the worst case is running
CI that was not strictly needed.

Usage:
  python determine_jobs.py
"""

from __future__ import annotations

import fnmatch
import json
import os
import subprocess
import sys
from pathlib import Path

_HERE = str(Path(__file__).resolve().parent)
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

from _gha import warn  # noqa: E402

# fnmatch globs (matched against a POSIX, repo-relative path) whose sole
# presence in a PR's diff means the Rust build and the lint/test gate add no
# signal. fnmatch's ``*`` spans ``/``, so ``*.md`` matches markdown at any depth.
#
# Deliberately conservative. Image and icon assets are omitted because
# src-tauri/icons feeds the build; workflow and dependabot files are omitted
# because a CI or dependency change must still build and test. When in doubt a
# path is NOT listed here, so it counts as code and runs CI.
META_ONLY_GLOBS = (
    "*.md",
    "LICENSE",
    ".gitignore",
    ".github/FUNDING.yml",
    ".github/ISSUE_TEMPLATE/*",
)


def is_meta_only(path: str) -> bool:
    """True if this single path is a documentation or repo-meta file."""
    return any(fnmatch.fnmatch(path, glob) for glob in META_ONLY_GLOBS)


def determine(changed: list[str]) -> dict[str, bool]:
    """Classify a set of changed repo-relative paths into per-job booleans.

    Pure so it can be unit tested without git or the GitHub API. An empty set
    (nothing detected) runs everything, matching the fail-safe bias.
    """
    run = (not changed) or any(not is_meta_only(path) for path in changed)
    # build.yml and lint-test.yml read these independently; keeping them as
    # separate keys lets the two gates diverge later without a workflow change.
    return {"build": run, "lint_test": run}


def _pr_number() -> str | None:
    """The pull request number from the GitHub Actions event payload, if any."""
    event_path = os.environ.get("GITHUB_EVENT_PATH")
    if not event_path or not os.path.exists(event_path):
        return None
    with open(event_path, encoding="utf-8") as handle:
        event = json.load(handle)
    number = event.get("pull_request", {}).get("number")
    return str(number) if number is not None else None


def _run(cmd: list[str]) -> list[str]:
    out = subprocess.run(cmd, capture_output=True, text=True, check=True).stdout
    return [line for line in out.splitlines() if line]


def changed_files() -> list[str]:
    """Best-effort list of the PR's changed files, repo-relative POSIX paths.

    Uses ``gh pr diff`` in Actions (with the paginated files API as a fallback
    for very large PRs), and a local ``git`` merge-base diff otherwise. Any
    failure raises to the caller, which turns it into a run-everything result.
    """
    if os.environ.get("GITHUB_ACTIONS") == "true" and (pr := _pr_number()):
        try:
            return _run(["gh", "pr", "diff", pr, "--name-only"])
        except subprocess.CalledProcessError as err:
            # gh refuses diffs over 300 files; fall back to the files API.
            if "maximum" not in (err.stderr or ""):
                raise
            repo = os.environ["GITHUB_REPOSITORY"]
            return _run(
                [
                    "gh",
                    "api",
                    f"repos/{repo}/pulls/{pr}/files",
                    "--paginate",
                    "--jq",
                    ".[].filename",
                ]
            )

    base = os.environ.get("GITHUB_BASE_REF") or "main"
    merge_base = _run(["git", "merge-base", f"origin/{base}", "HEAD"])[0]
    return _run(["git", "diff", "--name-only", f"{merge_base}...HEAD"])


def main() -> int:
    try:
        changed = changed_files()
    except Exception as err:  # noqa: BLE001 - detection must fail safe
        warn(f"Could not determine changed files ({err}); running all CI jobs")
        changed = []
    print(json.dumps(determine(changed)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
