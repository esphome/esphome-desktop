#!/usr/bin/env python3
"""Tests for .github/scripts/resolve_tauri_cli_version.py.

pytest suite, fully typed, no classes.
"""

from __future__ import annotations

import json
import runpy
import sys
from pathlib import Path

import pytest
from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "resolve_tauri_cli_version.py"
)

LOCK = """\
version = 3

[[package]]
name = "serde"
version = "1.0.0"

[[package]]
name = "tauri"
version = "2.12.2"
"""


def _index(*entries: tuple[str, bool]) -> str:
    return "\n".join(json.dumps({"vers": v, "yanked": y}) for v, y in entries) + "\n"


INDEX = _index(
    ("2.11.9", False),
    ("2.12.0", False),
    ("2.12.10", False),
    ("2.12.11", True),
    ("2.12.12-rc.1", False),
    ("2.13.0", False),
)


def test_locked_version_is_read_from_the_tauri_package() -> None:
    assert SCRIPT.locked_tauri_version(LOCK) == "2.12.2"


def test_repo_lock_resolves() -> None:
    text = (REPO_ROOT / "src-tauri" / "Cargo.lock").read_text(encoding="utf-8")
    assert SCRIPT.RELEASE_VERSION_RE.match(SCRIPT.locked_tauri_version(text))


@pytest.mark.parametrize(
    "lock",
    [
        "version = 3\n",
        LOCK + '\n[[package]]\nname = "tauri"\nversion = "2.12.3"\n',
        LOCK.replace("2.12.2", "2.12.2; rm -rf /"),
    ],
)
def test_locked_version_rejects_missing_duplicate_or_odd(lock: str) -> None:
    with pytest.raises(SCRIPT.ResolveError):
        SCRIPT.locked_tauri_version(lock)


def test_newest_patch_in_minor_skips_yanked_prerelease_and_other_minors() -> None:
    # Numeric, not lexical: 2.12.10 beats 2.12.0; yanked 2.12.11 and the rc are
    # skipped, and 2.13.0 is a different minor.
    assert SCRIPT.newest_cli_in_minor(INDEX, "2.12.2") == "2.12.10"


def test_blank_lines_in_the_index_are_ignored() -> None:
    assert SCRIPT.newest_cli_in_minor("\n" + INDEX + "\n\n", "2.11.0") == "2.11.9"


def test_no_matching_cli_is_an_error() -> None:
    with pytest.raises(SCRIPT.ResolveError, match="~2.14"):
        SCRIPT.newest_cli_in_minor(INDEX, "2.14.0")


def test_odd_library_version_is_an_error() -> None:
    with pytest.raises(SCRIPT.ResolveError):
        SCRIPT.newest_cli_in_minor(INDEX, "2.12")


def _write_inputs(tmp_path: Path, lock: str, index: str) -> tuple[Path, str]:
    lock_path = tmp_path / "Cargo.lock"
    lock_path.write_text(lock, encoding="utf-8")
    index_path = tmp_path / "index"
    index_path.write_text(index, encoding="utf-8")
    return lock_path, index_path.as_uri()


def test_main_emits_the_version_output(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    lock_path, index_url = _write_inputs(tmp_path, LOCK, INDEX)
    output = tmp_path / "github_output"
    monkeypatch.setenv("GITHUB_OUTPUT", str(output))
    assert SCRIPT.main(["--lock", str(lock_path), "--index-url", index_url]) == 0
    assert output.read_text(encoding="utf-8") == "version=2.12.10\n"


def test_main_reports_a_bad_index(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    lock_path, index_url = _write_inputs(tmp_path, LOCK, "not json\n")
    assert SCRIPT.main(["--lock", str(lock_path), "--index-url", index_url]) == 1
    assert "::error::could not resolve the tauri-cli version" in capsys.readouterr().err


def test_main_reports_a_missing_lock(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    missing = tmp_path / "missing.lock"
    assert SCRIPT.main(["--lock", str(missing), "--index-url", "file:///x"]) == 1
    assert "::error::" in capsys.readouterr().err


def test_entry_point_exits_with_mains_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    missing = tmp_path / "missing.lock"
    script = REPO_ROOT / ".github" / "scripts" / "resolve_tauri_cli_version.py"
    monkeypatch.setattr(sys, "argv", [str(script), "--lock", str(missing)])
    with pytest.raises(SystemExit) as exc:
        runpy.run_path(str(script), run_name="__main__")
    assert exc.value.code == 1
