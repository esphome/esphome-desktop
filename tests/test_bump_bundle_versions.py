#!/usr/bin/env python3
"""Tests for .github/scripts/bump_bundle_versions.py.

The nightly bump job rewrites the pinned interpreter and MinGit versions in
prepare_bundle.sh and opens a PR. A bug here could pin a non-existent version,
jump CPython minors (risking ESPHome/PlatformIO breakage), or mangle the script,
so the pure transforms and the upstream-resolution logic get a regression net.
Network access is monkeypatched out; nothing here touches GitHub.

pytest suite (maintainer-requested framework, fully typed, no classes).
"""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "bump_bundle_versions.py"

# A trimmed prepare_bundle.sh carrying the four pinned assignments the script
# touches, in the same shape as the real file.
SAMPLE_SCRIPT = """\
#!/bin/bash
set -e

PYTHON_VERSION="3.13.12"
PBS_VERSION="20260203"
BASE_URL="https://example/${PBS_VERSION}"

MINGIT_VERSION="2.54.0"
MINGIT_FILENAME="MinGit-${MINGIT_VERSION}-64-bit.zip"
MINGIT_SHA256="04f937e1f0918b17b9be6f2294cb2bb66e96e1d9832d1c298e2de088a1d0e668"
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
    assert bump.read_assignment(SAMPLE_SCRIPT, "MINGIT_VERSION") == "2.54.0"


def test_read_assignment_missing_raises_keyerror() -> None:
    with pytest.raises(KeyError):
        bump.read_assignment(SAMPLE_SCRIPT, "NOPE")


def test_has_assignment() -> None:
    assert bump.has_assignment(SAMPLE_SCRIPT, "PBS_VERSION")
    assert not bump.has_assignment(SAMPLE_SCRIPT, "MISSING")


def test_replace_assignment_only_touches_target_line() -> None:
    out = bump.replace_assignment(SAMPLE_SCRIPT, "PYTHON_VERSION", "3.13.13")
    assert 'PYTHON_VERSION="3.13.13"' in out
    # The MINGIT_FILENAME line embeds ${MINGIT_VERSION}; replacing a different
    # var must leave it (and everything else) untouched.
    assert 'MINGIT_FILENAME="MinGit-${MINGIT_VERSION}-64-bit.zip"' in out
    assert 'PBS_VERSION="20260203"' in out


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


def test_resolve_latest_mingit_prefers_digest(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    release = {
        "tag_name": "v2.55.1.windows.1",
        "assets": [
            {"name": "MinGit-2.55.1-64-bit.zip", "digest": "sha256:deadbeef"},
            {"name": "MinGit-2.55.1-busybox-64-bit.zip", "digest": "sha256:nope"},
        ],
    }
    monkeypatch.setattr(bump, "_api_get", lambda url: release)
    # Must not fall back to downloading when the digest is present.
    monkeypatch.setattr(
        bump,
        "_asset_sha256",
        lambda asset: asset["digest"].split(":", 1)[1],
    )
    assert bump.resolve_latest_mingit() == ("2.55.1", "deadbeef")


def test_resolve_latest_mingit_missing_asset_returns_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    release = {"tag_name": "v2.55.1.windows.1", "assets": []}
    monkeypatch.setattr(bump, "_api_get", lambda url: release)
    assert bump.resolve_latest_mingit() is None


def test_resolve_latest_mingit_unexpected_tag_returns_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setattr(bump, "_api_get", lambda url: {"tag_name": "weird", "assets": []})
    assert bump.resolve_latest_mingit() is None


def test_asset_sha256_reads_digest_without_network() -> None:
    asset = {"digest": "sha256:abc123", "browser_download_url": "http://unused"}
    assert bump._asset_sha256(asset) == "abc123"


# --------------------------------------------------------------------------- #
# CLI behaviour (no-op paths, output emission).
# --------------------------------------------------------------------------- #


def test_main_skips_when_variables_absent(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A prepare_bundle.sh with no MinGit vars (the state on main before #161).
    script = tmp_path / "prepare_bundle.sh"
    script.write_text('PYTHON_VERSION="3.13.12"\nPBS_VERSION="20260203"\n')
    out = tmp_path / "out"
    monkeypatch.setenv("GITHUB_OUTPUT", str(out))

    rc = bump.main(["--dependency", "mingit", "--file", str(script)])
    assert rc == 0
    assert "changed=false" in out.read_text()


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

    rc = bump.main(["--dependency", "python", "--file", str(script)])
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

    rc = bump.main(["--dependency", "python", "--file", str(script)])
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

    rc = bump.main(["--dependency", "python", "--file", str(script)])
    assert rc == 0
    assert script.read_text() == SAMPLE_SCRIPT
    assert "changed=false" in out.read_text()
