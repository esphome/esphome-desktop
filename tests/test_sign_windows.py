#!/usr/bin/env python3
"""Tests for build-scripts/sign_windows.ps1 (via its sign_windows.cmd shim).

The wrapper is Tauri's ``bundle.windows.signCommand`` on release builds and
nothing else ever runs it: PR builds do not sign, and a release build that
signs the wrong set of files either burns the Azure Artifact Signing quota
(every unsigned .exe/.dll in the bundled resource trees, ~190 files) or ships
an unsigned installer. Neither shows up until a release is already out, so the
allowlist, the no-credentials fallback and the retry loop are pinned here
against a stub ``artifact-signing-cli`` that records what it was asked to sign.

Windows only: the script's path handling is Windows-native and the shim is a
batch file. On other platforms the whole module is skipped, which is fine
because the scripts test workflow runs this suite on ``windows-latest`` too.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

import pytest

pytestmark = pytest.mark.skipif(
    sys.platform != "win32", reason="sign_windows.ps1 is Windows-only"
)

REPO_ROOT = Path(__file__).resolve().parent.parent
BUILD_SCRIPTS = REPO_ROOT / "build-scripts"

CREDENTIALS = {
    "AZURE_TENANT_ID": "tenant",
    "AZURE_CLIENT_ID": "client",
    "AZURE_CLIENT_SECRET": "secret",
    "AZURE_SIGNING_ENDPOINT": "https://weu.codesigning.azure.net",
    "AZURE_SIGNING_ACCOUNT": "acct",
    "AZURE_SIGNING_CERTIFICATE_PROFILE": "profile",
}

# What Tauri hands the sign command for a release build, relative to the
# fake repo root, and whether the wrapper is expected to sign it.
RESOURCE_FILES = [
    "src-tauri/python/DLLs/zlib1.dll",
    "src-tauri/python/Lib/venv/scripts/nt/venvlauncher.exe",
    "src-tauri/git/cmd/git.exe",
    "src-tauri/git/mingw64/bin/libcrypto-3-x64.dll",
    "src-tauri/ccache/ccache.exe",
]
SIGNED_FILES = [
    "src-tauri/python/python.exe",
    "src-tauri/python/pythonw.exe",
    "src-tauri/target/release/esphome-desktop.exe",
    "src-tauri/target/release/nsis/x64/uninstall.exe",
    "src-tauri/target/release/bundle/nsis/ESPHome Device Builder_1.0.0_x64-setup.exe",
]


@dataclass(frozen=True)
class FakeRepo:
    root: Path
    shim: Path
    stub_dir: Path
    calls_log: Path

    def path(self, relative: str) -> Path:
        return self.root / relative

    def calls(self) -> list[str]:
        if not self.calls_log.exists():
            return []
        return [
            line for line in self.calls_log.read_text().splitlines() if line.strip()
        ]


@pytest.fixture
def repo(tmp_path: Path) -> FakeRepo:
    """A repo-shaped tree: the real scripts copied in, PE files faked."""
    scripts = tmp_path / "build-scripts"
    scripts.mkdir()
    for name in ("sign_windows.ps1", "sign_windows.cmd"):
        shutil.copy(BUILD_SCRIPTS / name, scripts / name)
    for relative in RESOURCE_FILES + SIGNED_FILES:
        target = tmp_path / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(b"MZ")

    stub_dir = tmp_path / "stub"
    stub_dir.mkdir()
    calls_log = stub_dir / "calls.log"
    (stub_dir / "artifact-signing-cli.cmd").write_text(
        f'@echo off\r\necho %*>>"{calls_log}"\r\nexit /b %STUB_EXIT%\r\n'
    )
    return FakeRepo(
        root=tmp_path,
        shim=scripts / "sign_windows.cmd",
        stub_dir=stub_dir,
        calls_log=calls_log,
    )


def run_wrapper(
    repo: FakeRepo,
    relative: str,
    *,
    credentials: bool = True,
    stub_on_path: bool = True,
    stub_exit: int = 0,
) -> subprocess.CompletedProcess[str]:
    env = {k: v for k, v in os.environ.items() if not k.startswith("AZURE_")}
    if credentials:
        env.update(CREDENTIALS)
    if stub_on_path:
        env["PATH"] = f"{repo.stub_dir}{os.pathsep}{env.get('PATH', '')}"
    env["STUB_EXIT"] = str(stub_exit)
    return subprocess.run(
        [str(repo.shim), str(repo.path(relative))],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


@pytest.mark.parametrize("relative", RESOURCE_FILES)
def test_bundled_resource_trees_are_skipped(repo: FakeRepo, relative: str) -> None:
    result = run_wrapper(repo, relative)
    assert result.returncode == 0, result.stderr
    assert "skip (bundled resource)" in result.stdout
    assert repo.calls() == []


@pytest.mark.parametrize("relative", SIGNED_FILES)
def test_app_installer_and_interpreter_are_signed(
    repo: FakeRepo, relative: str
) -> None:
    result = run_wrapper(repo, relative)
    assert result.returncode == 0, result.stderr
    calls = repo.calls()
    assert len(calls) == 1
    call = calls[0]
    assert f"-e {CREDENTIALS['AZURE_SIGNING_ENDPOINT']}" in call
    assert f"-a {CREDENTIALS['AZURE_SIGNING_ACCOUNT']}" in call
    assert f"-c {CREDENTIALS['AZURE_SIGNING_CERTIFICATE_PROFILE']}" in call
    assert str(repo.path(relative)) in call


def test_without_credentials_the_file_is_left_unsigned(repo: FakeRepo) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0], credentials=False)
    assert result.returncode == 0, result.stderr
    assert "skip (no credentials" in result.stdout
    assert repo.calls() == []


def test_without_credentials_resources_stay_quiet(repo: FakeRepo) -> None:
    """Resource files never mention credentials; ~190 such lines would drown
    the one that matters."""
    result = run_wrapper(repo, RESOURCE_FILES[0], credentials=False)
    assert result.returncode == 0, result.stderr
    assert "skip (bundled resource)" in result.stdout
    assert "credentials" not in result.stdout


def test_missing_cli_fails_the_build(repo: FakeRepo) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0], stub_on_path=False)
    assert result.returncode != 0
    assert "artifact-signing-cli" in result.stderr + result.stdout


def test_missing_file_fails_the_build(repo: FakeRepo) -> None:
    result = run_wrapper(repo, "src-tauri/target/release/does-not-exist.exe")
    assert result.returncode != 0
    assert repo.calls() == []


def test_cli_failure_is_retried_then_fails(repo: FakeRepo) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0], stub_exit=1)
    assert result.returncode != 0
    assert len(repo.calls()) == 3
    assert "failed after 3 attempts" in result.stderr + result.stdout


def test_checkout_under_a_git_folder_is_not_mistaken_for_a_resource(
    tmp_path: Path,
) -> None:
    """The resource check is anchored on src-tauri/, not on a bare path
    segment, so a clone living under ...\\git\\... still gets its app signed."""
    nested = tmp_path / "git" / "python" / "checkout"
    nested.mkdir(parents=True)
    scripts = nested / "build-scripts"
    scripts.mkdir()
    for name in ("sign_windows.ps1", "sign_windows.cmd"):
        shutil.copy(BUILD_SCRIPTS / name, scripts / name)
    app = nested / "src-tauri" / "target" / "release" / "esphome-desktop.exe"
    app.parent.mkdir(parents=True)
    app.write_bytes(b"MZ")
    stub_dir = nested / "stub"
    stub_dir.mkdir()
    calls_log = stub_dir / "calls.log"
    (stub_dir / "artifact-signing-cli.cmd").write_text(
        f'@echo off\r\necho %*>>"{calls_log}"\r\nexit /b 0\r\n'
    )
    repo = FakeRepo(
        root=nested,
        shim=scripts / "sign_windows.cmd",
        stub_dir=stub_dir,
        calls_log=calls_log,
    )
    result = run_wrapper(repo, "src-tauri/target/release/esphome-desktop.exe")
    assert result.returncode == 0, result.stderr
    assert len(repo.calls()) == 1
