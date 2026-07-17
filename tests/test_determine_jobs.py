#!/usr/bin/env python3
"""Tests for .github/scripts/determine_jobs.py.

This gate decides whether a pull request runs the ~60 minute Tauri build
matrix and the Rust lint/test job, so the classifier's fail-safe bias is the
property that matters: it may only report "skip" when every changed file is a
documentation or repo-meta file, and must default to running CI for anything
else (including an empty or undetectable change set).

pytest suite, fully typed, no classes.
"""

from __future__ import annotations

from pathlib import Path

from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = load_script_module(REPO_ROOT / ".github" / "scripts" / "determine_jobs.py")


def test_docs_only_pr_skips_both_jobs() -> None:
    result = SCRIPT.determine(["CONTRIBUTING.md"])
    assert result == {"build": False, "lint_test": False}


def test_markdown_at_any_depth_counts_as_meta() -> None:
    assert SCRIPT.is_meta_only("src-tauri/README.md")
    assert not SCRIPT.determine(["docs/guide.md", "README.md"])["build"]


def test_license_and_meta_files_are_docs_only() -> None:
    for path in ("LICENSE", ".gitignore", ".github/FUNDING.yml"):
        assert SCRIPT.is_meta_only(path), path
    assert not SCRIPT.determine([".github/ISSUE_TEMPLATE/bug.yml"])["build"]


def test_any_code_file_forces_a_run() -> None:
    result = SCRIPT.determine(["src-tauri/src/platform/macos.rs"])
    assert result == {"build": True, "lint_test": True}


def test_mixed_docs_and_code_runs() -> None:
    # A PR touching a real source file must run even alongside docs edits.
    assert SCRIPT.determine(["CONTRIBUTING.md", "src-tauri/src/lib.rs"]) == {
        "build": True,
        "lint_test": True,
    }


def test_icon_and_workflow_files_are_not_treated_as_docs() -> None:
    # src-tauri/icons feeds the build, and a workflow/dependabot change must
    # still build and test, so none of these may be classified meta-only.
    for path in (
        "src-tauri/icons/icon.png",
        ".github/workflows/build.yml",
        ".github/dependabot.yml",
    ):
        assert not SCRIPT.is_meta_only(path), path
        assert SCRIPT.determine([path])["build"], path


def test_empty_change_set_fails_safe_to_running() -> None:
    assert SCRIPT.determine([]) == {"build": True, "lint_test": True}
