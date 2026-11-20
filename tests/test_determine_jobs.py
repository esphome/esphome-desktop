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


def test_web_folder_is_docs_only() -> None:
    # web/ is the desktop.esphome.io redirect page; changes there just
    # redeploy Pages and don't affect the Rust build or lint gate.
    assert SCRIPT.is_meta_only("web/index.html")
    assert SCRIPT.is_meta_only("web/assets/style.css")
    assert SCRIPT.determine(["web/index.html"]) == {
        "build": False,
        "lint_test": False,
    }


def test_deploy_pages_workflow_is_docs_only() -> None:
    # deploy-pages.yml is a targeted exception to the "workflow files count
    # as code" rule: it only publishes web/ to GitHub Pages.
    assert SCRIPT.is_meta_only(".github/workflows/deploy-pages.yml")
    assert SCRIPT.determine(
        ["web/index.html", ".github/workflows/deploy-pages.yml"]
    ) == {"build": False, "lint_test": False}


def test_python_tooling_only_pr_builds_but_skips_rust_lint() -> None:
    # Release tooling and its tests are gated by scripts-test.yml; they cannot
    # change the Rust lint/test result, but they still count as code for Build.
    for path in (
        ".github/scripts/generate_latest_json.py",
        "tests/test_generate_latest_json.py",
        "build-scripts/prepare_bundle.sh",
        ".github/workflows/scripts-test.yml",
        "pyproject.toml",
    ):
        assert not SCRIPT.affects_lint_test(path), path
        assert SCRIPT.determine([path]) == {"build": True, "lint_test": False}, path


def test_rust_gate_inputs_run_lint_test() -> None:
    for path in (
        "src-tauri/src/lib.rs",
        "src-tauri/Cargo.lock",
        "src-tauri/rust-toolchain.toml",
        "src-tauri/scripts/device_builder_maintenance.py",
        ".github/actions/setup-rust/action.yml",
        ".github/scripts/check_file_size.py",
        ".github/workflows/lint-test.yml",
    ):
        assert SCRIPT.affects_lint_test(path), path
        assert SCRIPT.determine([path]) == {"build": True, "lint_test": True}, path


def test_markdown_inside_src_tauri_does_not_run_lint_test() -> None:
    # The meta-only allowlist wins: a README under src-tauri/ is still docs.
    assert SCRIPT.determine(["src-tauri/README.md"]) == {
        "build": False,
        "lint_test": False,
    }


def test_rust_change_alongside_tooling_runs_lint_test() -> None:
    assert SCRIPT.determine(["tests/test_set_version.py", "src-tauri/src/lib.rs"]) == {
        "build": True,
        "lint_test": True,
    }
