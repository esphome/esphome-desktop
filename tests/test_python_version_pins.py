#!/usr/bin/env python3
"""CI's Python must be the Python the desktop app bundles.

`.python-version` is the single CI pin: setup-python reads it through
`python-version-file`, and uv (setup-uv, `uv run`) reads it on its own. These
tests fail when it drifts from PYTHON_VERSION in prepare_bundle.sh, when
pyproject.toml's requires-python names another minor, when a workflow sets its
own version instead of reading the file, or when a job runs Python on the
runner's unpinned interpreter (ruff targets the pinned version, so the repo's
scripts may use syntax an older Python can't parse).
"""

from __future__ import annotations

import re
import tomllib
from pathlib import Path

import pytest
import yaml

REPO_ROOT = Path(__file__).resolve().parent.parent
WORKFLOWS = sorted((REPO_ROOT / ".github" / "workflows").glob("*.yml"))

# A shell command that starts the Python interpreter (`python3 x.py`,
# `$(python3 ...)`), but not a path segment like `src-tauri/python`.
_PYTHON_CALL_RE = re.compile(r"(?:^|[\s;|&(])python3?(?:\s|$)", re.MULTILINE)
_SETUP_ACTIONS = ("actions/setup-python@", "astral-sh/setup-uv@")

_PYTHON_VERSION_RE = re.compile(r'^PYTHON_VERSION="([^"]+)"$', re.MULTILINE)


def _pinned() -> str:
    return (REPO_ROOT / ".python-version").read_text(encoding="utf-8").strip()


def test_python_version_matches_bundled_python() -> None:
    text = (REPO_ROOT / "build-scripts" / "prepare_bundle.sh").read_text(
        encoding="utf-8"
    )
    match = _PYTHON_VERSION_RE.search(text)
    assert match is not None, "PYTHON_VERSION not found in prepare_bundle.sh"
    assert _pinned() == match.group(1)


def test_requires_python_matches_minor() -> None:
    with (REPO_ROOT / "pyproject.toml").open("rb") as f:
        requires = tomllib.load(f)["project"]["requires-python"]
    minor = ".".join(_pinned().split(".")[:2])
    assert requires == f">={minor}"


def test_workflows_found() -> None:
    # Guards the parametrized test below against silently checking nothing.
    assert WORKFLOWS


@pytest.mark.parametrize("workflow", WORKFLOWS, ids=lambda p: p.name)
def test_workflow_reads_python_version_file(workflow: Path) -> None:
    text = workflow.read_text(encoding="utf-8")
    assert not re.search(r"^\s*python-version:", text, re.MULTILINE), (
        f"{workflow.name} pins its own Python; use "
        "`python-version-file: .python-version` instead"
    )
    setups = len(re.findall(r"uses:\s*actions/setup-python@", text))
    files = len(re.findall(r"python-version-file:\s*\S*\.python-version", text))
    assert setups == files, (
        f"{workflow.name}: every actions/setup-python step needs "
        "`python-version-file: .python-version`"
    )


def _unpinned_python_steps(workflow: dict) -> list[str]:
    """Steps that run `python`/`python3` before the job sets up Python."""
    found: list[str] = []
    for job_id, job in (workflow.get("jobs") or {}).items():
        pinned = False
        for step in job.get("steps") or []:
            if str(step.get("uses", "")).startswith(_SETUP_ACTIONS):
                pinned = True
            elif not pinned and _PYTHON_CALL_RE.search(str(step.get("run", ""))):
                found.append(f"{job_id}: {step.get('name', '<unnamed>')}")
    return found


def test_unpinned_python_steps_detection() -> None:
    workflow = {
        "jobs": {
            "late": {
                "steps": [
                    {"name": "early", "run": "out=$(python3 a.py)"},
                    {"uses": "actions/setup-python@v7.0.0"},
                    {"name": "after", "run": "python3 b.py"},
                ]
            },
            "uv": {
                "steps": [
                    {"uses": "astral-sh/setup-uv@v10.3.3"},
                    {"run": "python c.py"},
                ]
            },
            "path-only": {"steps": [{"run": "cp -r src-tauri/python out"}]},
            "reusable": {"uses": "./.github/workflows/x.yml"},
        }
    }
    assert _unpinned_python_steps(workflow) == ["late: early"]


@pytest.mark.parametrize("workflow", WORKFLOWS, ids=lambda p: p.name)
def test_workflow_runs_python_only_after_setup(workflow: Path) -> None:
    data = yaml.safe_load(workflow.read_text(encoding="utf-8"))
    assert _unpinned_python_steps(data) == [], (
        f"{workflow.name}: add a setup-python step "
        "(`python-version-file: .python-version`) before these steps"
    )
