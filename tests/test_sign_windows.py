#!/usr/bin/env python3
"""Tests for build-scripts/sign_windows.ps1 (via its sign_windows.cmd shim).

The wrapper is Tauri's ``bundle.windows.signCommand`` on release builds and
nothing else ever runs it: PR builds do not sign, and a release build that
signs the wrong set of files either burns the Azure Artifact Signing quota
(every unsigned .exe/.dll in the bundled resource trees, ~190 files) or ships
an unsigned installer. Neither shows up until a release is already out, so the
allowlist, the no-credentials fallback, the signtool invocation and the retry
loop are pinned here against a stub ``signtool.exe`` that records what it was
asked to sign. The stub is reached through the wrapper's own
``SIGN_WINDOWS_SIGNTOOL`` override, never through ``PATH``, so nothing installed
on the machine can leak into these tests.

Windows only: the script's path handling is Windows-native and the shim is a
batch file. On other platforms the whole module is skipped, which is fine
because the scripts test workflow runs this suite on ``windows-latest`` too.
"""

from __future__ import annotations

import json
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
    signtool: Path
    dlib: Path
    calls_log: Path

    def path(self, relative: str) -> Path:
        return self.root / relative

    def calls(self) -> list[str]:
        if not self.calls_log.exists():
            return []
        return [
            line for line in self.calls_log.read_text().splitlines() if line.strip()
        ]

    def metadata(self) -> dict[str, object]:
        # The wrapper writes the dlib's metadata file into RUNNER_TEMP, which
        # run_wrapper points at the fake repo root.
        return json.loads((self.root / "sign_windows-metadata.json").read_text())


def make_repo(root: Path) -> FakeRepo:
    """A repo-shaped tree at ``root``: the real scripts copied in, PE files
    faked, a fake dlib, and a stub signtool that logs its arguments."""
    root.mkdir(parents=True, exist_ok=True)
    scripts = root / "build-scripts"
    scripts.mkdir()
    for name in ("sign_windows.ps1", "sign_windows.cmd"):
        shutil.copy(BUILD_SCRIPTS / name, scripts / name)
    for relative in RESOURCE_FILES + SIGNED_FILES:
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(b"MZ")

    stub_dir = root / "stub"
    stub_dir.mkdir()
    calls_log = stub_dir / "calls.log"
    signtool = stub_dir / "signtool.cmd"
    signtool.write_text(
        f'@echo off\r\necho %*>>"{calls_log}"\r\nexit /b %STUB_EXIT%\r\n'
    )
    dlib = stub_dir / "Azure.CodeSigning.Dlib.dll"
    dlib.write_bytes(b"MZ")
    return FakeRepo(
        root=root,
        shim=scripts / "sign_windows.cmd",
        signtool=signtool,
        dlib=dlib,
        calls_log=calls_log,
    )


@pytest.fixture
def repo(tmp_path: Path) -> FakeRepo:
    return make_repo(tmp_path)


def run_wrapper(
    repo: FakeRepo,
    relative: str,
    *,
    credentials: bool = True,
    signtool: Path | None = None,
    dlib: Path | None = None,
    stub_exit: int = 0,
) -> subprocess.CompletedProcess[str]:
    env = {
        k: v
        for k, v in os.environ.items()
        if not k.startswith(("AZURE_", "SIGN_WINDOWS_", "RUNNER_TEMP"))
    }
    if credentials:
        env.update(CREDENTIALS)
    env["SIGN_WINDOWS_SIGNTOOL"] = str(signtool or repo.signtool)
    env["SIGN_WINDOWS_DLIB"] = str(dlib or repo.dlib)
    env["RUNNER_TEMP"] = str(repo.root)
    env["STUB_EXIT"] = str(stub_exit)
    # Real backoff is 5 s then 10 s; the retry test only cares about the count.
    env["SIGN_WINDOWS_RETRY_DELAY"] = "0"
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
    assert call.startswith("sign /v /fd SHA256 ")
    assert "/tr http://timestamp.acs.microsoft.com /td SHA256" in call
    assert f"/dlib {repo.dlib}" in call
    assert f"/dmdf {repo.root / 'sign_windows-metadata.json'}" in call
    # PowerShell quotes an argument with spaces (the installer name has them),
    # so the logged line may end in a closing quote.
    assert call.rstrip('"').endswith(str(repo.path(relative)))


def test_metadata_names_the_account_and_pins_environment_credential(
    repo: FakeRepo,
) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0])
    assert result.returncode == 0, result.stderr
    metadata = repo.metadata()
    assert metadata["Endpoint"] == CREDENTIALS["AZURE_SIGNING_ENDPOINT"]
    assert metadata["CodeSigningAccountName"] == CREDENTIALS["AZURE_SIGNING_ACCOUNT"]
    assert (
        metadata["CertificateProfileName"]
        == CREDENTIALS["AZURE_SIGNING_CERTIFICATE_PROFILE"]
    )
    excluded = metadata["ExcludeCredentials"]
    assert isinstance(excluded, list)
    assert "EnvironmentCredential" not in excluded
    assert "AzureCliCredential" in excluded
    assert "ManagedIdentityCredential" in excluded


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


def test_missing_signtool_fails_the_build(repo: FakeRepo) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0], signtool=repo.root / "nope.exe")
    assert result.returncode != 0
    assert "signtool.exe not found" in result.stderr + result.stdout
    assert repo.calls() == []


def test_missing_dlib_fails_the_build(repo: FakeRepo) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0], dlib=repo.root / "nope.dll")
    assert result.returncode != 0
    assert "SIGN_WINDOWS_DLIB" in result.stderr + result.stdout
    assert repo.calls() == []


def test_missing_file_fails_the_build(repo: FakeRepo) -> None:
    result = run_wrapper(repo, "src-tauri/target/release/does-not-exist.exe")
    assert result.returncode != 0
    assert repo.calls() == []


def test_signtool_failure_is_retried_then_fails(repo: FakeRepo) -> None:
    result = run_wrapper(repo, SIGNED_FILES[0], stub_exit=1)
    assert result.returncode != 0
    assert len(repo.calls()) == 3
    assert "failed after 3 attempts" in result.stderr + result.stdout


def test_checkout_under_a_git_folder_is_not_mistaken_for_a_resource(
    tmp_path: Path,
) -> None:
    """The resource check is anchored on src-tauri/, not on a bare path
    segment, so a clone living under ...\\git\\... still gets its app signed."""
    repo = make_repo(tmp_path / "git" / "python" / "checkout")
    result = run_wrapper(repo, "src-tauri/target/release/esphome-desktop.exe")
    assert result.returncode == 0, result.stderr
    assert len(repo.calls()) == 1
    result = run_wrapper(repo, RESOURCE_FILES[0])
    assert result.returncode == 0, result.stderr
    assert "skip (bundled resource)" in result.stdout
    assert len(repo.calls()) == 1
