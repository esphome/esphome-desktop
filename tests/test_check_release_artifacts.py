#!/usr/bin/env python3
"""Tests for .github/scripts/check_release_artifacts.py.

Whatever passes this check is uploaded to the draft release, so it must
accept exactly the expected file set and reject anything missing, duplicated
or extra.

pytest suite, fully typed, no classes.
"""

from __future__ import annotations

import os
import runpy
import sys
from collections.abc import Callable
from pathlib import Path

import pytest
from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "check_release_artifacts.py"
)
GENERATOR = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "generate_latest_json.py"
)

P = "ESPHome.Device.Builder"
LINUX = [
    f"{P}_1.2.1_amd64.AppImage",
    f"{P}_1.2.1_aarch64.AppImage",
    f"{P}_1.2.1_amd64.deb",
    f"{P}_1.2.1_arm64.deb",
    f"{P}-1.2.1-1.x86_64.rpm",
    f"{P}-1.2.1-1.aarch64.rpm",
]
PAYLOADS = [
    f"{P}_1.2.1_x64-setup.exe",
    *LINUX,
    f"{P}_aarch64.app.tar.gz",
    f"{P}_x64.app.tar.gz",
]
RELEASE = [
    *PAYLOADS,
    *(f"{name}.sig" for name in PAYLOADS),
    f"{P}_1.2.1_aarch64.dmg",
    f"{P}_1.2.1_x64.dmg",
]


LINUX_SIGS = {f"{name}.sig" for name in LINUX}


def _populate(root: Path, names: list[str]) -> Path:
    """Lay files out as download-artifact does without merge-multiple.

    Each single-file artifact gets a directory named after its file; the
    Linux updater signatures arrive together in one zipped artifact.
    """
    for name in names:
        folder = "linux-updater-signatures" if name in LINUX_SIGS else name
        target = root / folder / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(b"x")
    return root


def test_full_release_set_passes(tmp_path: Path) -> None:
    assert SCRIPT.main(["release", str(_populate(tmp_path, RELEASE))]) == 0


def test_linux_bundle_set_passes(tmp_path: Path) -> None:
    assert SCRIPT.main(["linux-bundles", str(_populate(tmp_path, LINUX))]) == 0


def test_every_latest_json_asset_is_expected() -> None:
    # Anything latest.json links to has to make it past this check.
    for _, pattern in GENERATOR.PLATFORM_SIG_MATCHERS:
        assert any(pattern.match(name) for name in RELEASE), pattern.pattern
    for _, _, pattern in GENERATOR.DOWNLOAD_MATCHERS:
        assert any(pattern.search(name) for name in RELEASE), pattern.pattern


@pytest.mark.parametrize(
    ("names", "problem"),
    [
        ([n for n in RELEASE if not n.endswith("-setup.exe.sig")], "missing windows"),
        ([*RELEASE, f"{P}_1.2.2_x64-setup.exe"], "more than one windows"),
        ([*RELEASE, "de.json"], "unexpected file: de.json"),
        ([*RELEASE, "intermediate-windows-x64-unbundled.tar"], "unexpected file"),
        (RELEASE[:-1], "missing macos x64 DMG"),
    ],
)
def test_missing_duplicate_or_extra_files_fail(
    tmp_path: Path,
    names: list[str],
    problem: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
    assert SCRIPT.main(["release", str(_populate(tmp_path, names))]) == 1
    assert problem in capsys.readouterr().err


def test_signed_linux_bundles_are_not_accepted_as_unsigned(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    names = [*LINUX, f"{LINUX[0]}.sig"]
    assert SCRIPT.main(["linux-bundles", str(_populate(tmp_path, names))]) == 1
    assert "unexpected file" in capsys.readouterr().err


@pytest.mark.skipif(os.name == "nt", reason="symlinks need privileges on Windows")
def test_symlinks_are_rejected(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _populate(tmp_path / "artifacts", LINUX[1:])
    (root / LINUX[0]).mkdir()
    (root / LINUX[0] / LINUX[0]).symlink_to(tmp_path / "elsewhere")
    (tmp_path / "elsewhere").write_bytes(b"x")
    assert SCRIPT.main(["linux-bundles", str(root)]) == 1
    assert "not a regular file" in capsys.readouterr().err


def _move_into_artifact(root: Path, name: str, artifact: str) -> None:
    """Put `name` inside another artifact's directory instead of its own."""
    (root / artifact).mkdir(exist_ok=True)
    (root / name / name).rename(root / artifact / name)
    (root / name).rmdir()


@pytest.mark.parametrize(
    ("arrange", "problem"),
    [
        # The poisoning case: an artifact a build leg uploaded as `x.sig`
        # carries a file named like the signed installer. Every expected name
        # is still present exactly once, so only the layout check sees it.
        (
            lambda root: _move_into_artifact(root, PAYLOADS[0], "x.sig"),
            "artifact x.sig holds",
        ),
        (
            lambda root: (root / "linux-updater-signatures" / "evil.exe").write_bytes(
                b"x"
            ),
            "linux-updater-signatures holds unexpected files",
        ),
        (
            lambda root: (
                [p.unlink() for p in (root / "linux-updater-signatures").iterdir()]
                and None
            ),
            "linux-updater-signatures holds unexpected files",
        ),
        (
            lambda root: (root / PAYLOADS[1] / "sub").mkdir(),
            "holds nested paths",
        ),
        (
            lambda root: (root / "stray.dmg").write_bytes(b"x"),
            "not an artifact directory: stray.dmg",
        ),
    ],
    ids=[
        "file-in-foreign-artifact",
        "wrong-file-in-zipped-artifact",
        "empty-zipped-artifact",
        "nested-path",
        "file-at-top-level",
    ],
)
def test_artifact_layout_is_enforced(
    tmp_path: Path,
    arrange: Callable[[Path], object],
    problem: str,
    capsys: pytest.CaptureFixture[str],
) -> None:
    root = _populate(tmp_path / "artifacts", RELEASE)
    arrange(root)
    assert SCRIPT.main(["release", str(root)]) == 1
    assert problem in capsys.readouterr().err


def test_missing_directory_fails(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    assert SCRIPT.main(["release", str(tmp_path / "nope")]) == 1
    assert "is not a directory" in capsys.readouterr().err


def test_entry_point_exits_with_mains_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    script = REPO_ROOT / ".github" / "scripts" / "check_release_artifacts.py"
    root = _populate(tmp_path, LINUX)
    monkeypatch.setattr(sys, "argv", [str(script), "linux-bundles", str(root)])
    with pytest.raises(SystemExit) as exc:
        runpy.run_path(str(script), run_name="__main__")
    assert exc.value.code == 0
