#!/usr/bin/env python3
"""sys.path setup in the release-pipeline scripts the workflows run.

Each script puts its own directory on sys.path so the sibling _gha module
imports regardless of the caller's cwd. Loading a script again must not add
the directory a second time, so both branches of the guard have to run.

pytest suite, fully typed, no classes.
"""

from __future__ import annotations

import sys
from pathlib import Path

import pytest
from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPTS = REPO_ROOT / ".github" / "scripts"


@pytest.mark.parametrize(
    "name",
    [
        "check_release_artifacts.py",
        "resolve_tauri_cli_version.py",
        "restore_unbundled_app.py",
        "verify_updater_signatures.py",
    ],
)
def test_script_dir_sys_path_insertion_is_idempotent(name: str) -> None:
    script = SCRIPTS / name
    script_dir = str(script.resolve().parent)
    original_path = list(sys.path)
    try:
        while script_dir in sys.path:
            sys.path.remove(script_dir)
        load_script_module(script)  # absent -> inserted
        assert sys.path.count(script_dir) == 1
        load_script_module(script)  # already present -> insert skipped
        assert sys.path.count(script_dir) == 1
    finally:
        sys.path[:] = original_path
