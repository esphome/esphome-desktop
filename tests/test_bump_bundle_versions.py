#!/usr/bin/env python3
"""Tests for .github/scripts/bump_bundle_versions.py.

The nightly bump job rewrites the pinned bundled Python version in
prepare_bundle.sh and opens a PR. A bug here could pin a non-existent version,
jump CPython minors (risking ESPHome/PlatformIO breakage), or mangle the script,
so the pure transforms and the upstream-resolution logic get a regression net.
Network access is monkeypatched out; nothing here touches GitHub.

pytest suite (maintainer-requested framework, fully typed, no classes).
"""

from __future__ import annotations

import importlib.util
import sys
import urllib.error
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "bump_bundle_versions.py"

# A trimmed prepare_bundle.sh carrying the assignments the script touches, in
# the same shape as the real file.
SAMPLE_SCRIPT = """\
#!/bin/bash
set -e

PYTHON_VERSION="3.13.12"
PBS_VERSION="20260203"
BASE_URL="https://example/${PBS_VERSION}"

MINGIT_VERSION="2.53.0"
MINGIT_URL="https://github.com/git-for-windows/git/releases/download/v2.53.0.windows.1/MinGit-2.53.0-64-bit.zip"
MINGIT_SHA256="0000000000000000000000000000000000000000000000000000000000000000"
"""


def _load_module() -> ModuleType:
    spec = importlib.util.spec_from_file_location("bump_bundle_versions", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    # Register before exec so dataclasses can resolve the module's annotations
    # (it looks the module up in sys.modules during class processing).
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


bump = _load_module()


# --------------------------------------------------------------------------- #
# Pure transforms.
# --------------------------------------------------------------------------- #


def test_read_assignment_returns_quoted_value() -> None:
    assert bump.read_assignment(SAMPLE_SCRIPT, "PYTHON_VERSION") == "3.13.12"
    assert bump.read_assignment(SAMPLE_SCRIPT, "PBS_VERSION") == "20260203"


def test_read_assignment_missing_raises_keyerror() -> None:
    with pytest.raises(KeyError):
        bump.read_assignment(SAMPLE_SCRIPT, "NOPE")


def test_has_assignment() -> None:
    assert bump.has_assignment(SAMPLE_SCRIPT, "PBS_VERSION")
    assert not bump.has_assignment(SAMPLE_SCRIPT, "MISSING")


def test_replace_assignment_only_touches_target_line() -> None:
    out = bump.replace_assignment(SAMPLE_SCRIPT, "PYTHON_VERSION", "3.13.13")
    assert 'PYTHON_VERSION="3.13.13"' in out
    # Replacing one var must leave every other assignment untouched, including
    # the BASE_URL line that embeds ${PBS_VERSION}.
    assert 'PBS_VERSION="20260203"' in out
    assert 'BASE_URL="https://example/${PBS_VERSION}"' in out


def test_replace_assignment_missing_raises() -> None:
    with pytest.raises(KeyError):
        bump.replace_assignment(SAMPLE_SCRIPT, "MISSING", "x")


def test_current_python_minor() -> None:
    assert bump.current_python_minor(SAMPLE_SCRIPT) == "3.13"


def test_apply_bumps_reports_only_moved_vars() -> None:
    result = bump.apply_bumps(
        SAMPLE_SCRIPT,
        {"PYTHON_VERSION": "3.13.13", "PBS_VERSION": "20260203"},
    )
    assert result.changed
    # PBS_VERSION was already current, so only PYTHON_VERSION is recorded.
    assert result.var_changes == {"PYTHON_VERSION": ("3.13.12", "3.13.13")}
    assert 'PYTHON_VERSION="3.13.13"' in result.text


def test_apply_bumps_no_change_is_not_changed() -> None:
    result = bump.apply_bumps(SAMPLE_SCRIPT, {"PYTHON_VERSION": "3.13.12"})
    assert not result.changed
    assert result.var_changes == {}
    assert result.text == SAMPLE_SCRIPT


# --------------------------------------------------------------------------- #
# Network retry behaviour.
# --------------------------------------------------------------------------- #


def test_with_retries_returns_on_first_success() -> None:
    calls = []

    def op() -> str:
        calls.append(1)
        return "ok"

    assert bump._with_retries(op) == "ok"
    assert len(calls) == 1


def test_with_retries_recovers_after_transient_failures(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(bump.time, "sleep", lambda _s: None)

    attempts = {"n": 0}

    def flaky() -> str:
        attempts["n"] += 1
        if attempts["n"] < bump.HTTP_RETRIES:
            raise urllib.error.URLError("transient")
        return "recovered"

    assert bump._with_retries(flaky) == "recovered"
    assert attempts["n"] == bump.HTTP_RETRIES


def test_with_retries_reraises_persistent_failure(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A persistent failure must propagate so the nightly job fails loudly
    # rather than silently reporting "nothing to bump" and letting the bundled
    # Python drift.
    monkeypatch.setattr(bump.time, "sleep", lambda _s: None)

    attempts = {"n": 0}

    def always_fail() -> str:
        attempts["n"] += 1
        raise urllib.error.URLError("down")

    with pytest.raises(urllib.error.URLError):
        bump._with_retries(always_fail)
    assert attempts["n"] == bump.HTTP_RETRIES


# --------------------------------------------------------------------------- #
# Upstream resolution (monkeypatched network).
# --------------------------------------------------------------------------- #


def _pbs_release() -> dict[str, Any]:
    """A python-build-standalone release shaped like the real API payload."""
    names = [
        # Two 3.13 patches: the resolver must pick the higher one.
        "cpython-3.13.12+20260602-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
        "cpython-3.13.13+20260602-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
        # A newer minor that must be ignored under the patch-only policy.
        "cpython-3.14.1+20260602-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
        # A different arch of 3.13 that the Linux-x86_64 regex shouldn't match.
        "cpython-3.13.99+20260602-aarch64-apple-darwin-install_only_stripped.tar.gz",
    ]
    return {
        "tag_name": "20260602",
        "assets": [{"name": n} for n in names],
    }


def test_resolve_latest_python_picks_highest_patch_in_minor(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(bump, "_api_get", lambda url: _pbs_release())
    assert bump.resolve_latest_python("3.13") == ("20260602", "3.13.13")


def test_resolve_latest_python_ignores_other_minor(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # 3.12 has no build in this release, so we skip rather than jump to 3.13/3.14.
    monkeypatch.setattr(bump, "_api_get", lambda url: _pbs_release())
    assert bump.resolve_latest_python("3.12") is None


def _gfw_release(tag: str, mingit_ver: str) -> dict[str, Any]:
    """A git-for-windows release shaped like the real API payload.

    The plain 64-bit MinGit zip must be chosen over the busybox, arm64 and
    32-bit siblings, and the version token must come straight from its name (so
    a `.windows.N` rebuild's `MinGit-2.53.0.3-64-bit.zip` resolves to 2.53.0.3).
    """
    base = f"https://github.com/git-for-windows/git/releases/download/{tag}"
    names = [
        f"MinGit-{mingit_ver}-32-bit.zip",
        f"MinGit-{mingit_ver}-arm64.zip",
        f"MinGit-{mingit_ver}-busybox-64-bit.zip",
        f"MinGit-{mingit_ver}-64-bit.zip",
    ]
    return {
        "tag_name": tag,
        "assets": [
            {
                "name": n,
                "browser_download_url": f"{base}/{n}",
                "digest": f"sha256:{'ab' * 32}",
            }
            for n in names
        ],
    }


def test_resolve_latest_mingit_picks_plain_64bit_asset(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        bump, "_api_get", lambda url: _gfw_release("v2.54.0.windows.1", "2.54.0")
    )
    version, url, sha = bump.resolve_latest_mingit()
    assert version == "2.54.0"
    assert url.endswith("/v2.54.0.windows.1/MinGit-2.54.0-64-bit.zip")
    assert sha == "ab" * 32


def test_resolve_latest_mingit_handles_rebuild(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A .windows.N rebuild encodes the build number in the filename; the literal
    # URL and the version both come from the asset, not a reconstruction.
    monkeypatch.setattr(
        bump, "_api_get", lambda url: _gfw_release("v2.53.0.windows.3", "2.53.0.3")
    )
    version, url, _sha = bump.resolve_latest_mingit()
    assert version == "2.53.0.3"
    assert url.endswith("/v2.53.0.windows.3/MinGit-2.53.0.3-64-bit.zip")


def test_resolve_latest_mingit_raises_when_no_asset(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(
        bump, "_api_get", lambda url: {"tag_name": "v2.54.0.windows.1", "assets": []}
    )
    with pytest.raises(bump.ResolutionError):
        bump.resolve_latest_mingit()


def test_asset_sha256_prefers_digest_without_download(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def _no_download(url: str) -> str:
        raise AssertionError("should not download when a digest is present")

    monkeypatch.setattr(bump, "_download_sha256", _no_download)
    assert bump._asset_sha256({"digest": f"sha256:{'cd' * 32}"}) == "cd" * 32


def test_asset_sha256_falls_back_to_download(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(bump, "_download_sha256", lambda url: "ef" * 32)
    asset = {"digest": None, "browser_download_url": "https://example/MinGit.zip"}
    assert bump._asset_sha256(asset) == "ef" * 32


# --------------------------------------------------------------------------- #
# CLI behaviour (failure paths, output emission).
# --------------------------------------------------------------------------- #


def test_main_fails_when_variables_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # PBS_VERSION must exist; its absence is a real breakage and must fail the
    # job (non-zero) rather than silently no-op, which would let the bundled
    # Python drift unnoticed.
    script = tmp_path / "prepare_bundle.sh"
    script.write_text('PYTHON_VERSION="3.13.12"\n')
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["--file", str(script)])
    assert rc == 1
    # A hard failure writes no outputs, so the create-PR step never fires.
    assert not out.exists() or "changed=true" not in out.read_text()


def test_main_fails_when_upstream_unresolvable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Resolver returning None (no build for the pinned minor) is a broken
    # assumption, not a routine skip, so main must fail loudly.
    script = tmp_path / "prepare_bundle.sh"
    script.write_text(SAMPLE_SCRIPT)
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    monkeypatch.setattr(bump, "resolve_latest_python", lambda minor: None)

    rc = bump.main(["--file", str(script)])
    assert rc == 1
    assert not out.exists() or "changed=true" not in out.read_text()


def test_main_writes_file_and_outputs_on_bump(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    script = tmp_path / "prepare_bundle.sh"
    script.write_text(SAMPLE_SCRIPT)
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    monkeypatch.setattr(
        bump, "resolve_latest_python", lambda minor: ("20260602", "3.13.13")
    )

    rc = bump.main(["--file", str(script)])
    assert rc == 0
    assert 'PYTHON_VERSION="3.13.13"' in script.read_text()
    assert 'PBS_VERSION="20260602"' in script.read_text()

    output = out.read_text()
    assert "changed=true" in output
    assert "Bump bundled Python to 3.13.13" in output


def test_main_build_only_bump_titles_the_build_not_the_version(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # New PBS release, same CPython patch: only PBS_VERSION moves, so the title
    # must name the build rather than claim a (non-existent) version bump.
    script = tmp_path / "prepare_bundle.sh"
    script.write_text(SAMPLE_SCRIPT)
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    monkeypatch.setattr(
        bump, "resolve_latest_python", lambda minor: ("20260602", "3.13.12")
    )

    rc = bump.main(["--file", str(script)])
    assert rc == 0
    assert 'PBS_VERSION="20260602"' in script.read_text()
    assert 'PYTHON_VERSION="3.13.12"' in script.read_text()

    output = out.read_text()
    assert "changed=true" in output
    assert "Bump bundled Python build to 20260602 (3.13.12)" in output


def test_main_no_op_when_already_current(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    script = tmp_path / "prepare_bundle.sh"
    script.write_text(SAMPLE_SCRIPT)
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    monkeypatch.setattr(
        bump, "resolve_latest_python", lambda minor: ("20260203", "3.13.12")
    )

    rc = bump.main(["--file", str(script)])
    assert rc == 0
    assert script.read_text() == SAMPLE_SCRIPT
    assert "changed=false" in out.read_text()


def test_main_mingit_writes_file_and_outputs_on_bump(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    script = tmp_path / "prepare_bundle.sh"
    script.write_text(SAMPLE_SCRIPT)
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    new_url = (
        "https://github.com/git-for-windows/git/releases/download/"
        "v2.54.0.windows.1/MinGit-2.54.0-64-bit.zip"
    )
    monkeypatch.setattr(
        bump, "resolve_latest_mingit", lambda: ("2.54.0", new_url, "ff" * 32)
    )

    rc = bump.main(["--target", "mingit", "--file", str(script)])
    assert rc == 0
    text = script.read_text()
    assert 'MINGIT_VERSION="2.54.0"' in text
    assert f'MINGIT_URL="{new_url}"' in text
    assert f'MINGIT_SHA256="{"ff" * 32}"' in text
    # The Python pins are untouched by a MinGit bump.
    assert 'PYTHON_VERSION="3.13.12"' in text

    output = out.read_text()
    assert "changed=true" in output
    assert "Bump bundled MinGit to 2.54.0" in output


def test_main_mingit_no_op_when_already_current(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    script = tmp_path / "prepare_bundle.sh"
    script.write_text(SAMPLE_SCRIPT)
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))
    current_url = (
        "https://github.com/git-for-windows/git/releases/download/"
        "v2.53.0.windows.1/MinGit-2.53.0-64-bit.zip"
    )
    monkeypatch.setattr(
        bump, "resolve_latest_mingit", lambda: ("2.53.0", current_url, "0" * 64)
    )

    rc = bump.main(["--target", "mingit", "--file", str(script)])
    assert rc == 0
    assert script.read_text() == SAMPLE_SCRIPT
    assert "changed=false" in out.read_text()


def test_main_mingit_fails_when_variables_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # No MinGit pins present: a real breakage that must fail the job rather than
    # silently no-op and let the bundled git drift.
    script = tmp_path / "prepare_bundle.sh"
    script.write_text('PYTHON_VERSION="3.13.12"\nPBS_VERSION="20260203"\n')
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["--target", "mingit", "--file", str(script)])
    assert rc == 1
    assert not out.exists() or "changed=true" not in out.read_text()
