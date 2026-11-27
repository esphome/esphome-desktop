#!/usr/bin/env python3
"""Tests for .github/scripts/restore_unbundled_app.py.

The tarball comes from a build job that runs third-party code and is unpacked
into a job that holds signing credentials, so the property that matters is
that nothing outside the app binary and the resource trees can be written, and
no link can point out of its tree.

pytest suite, fully typed, no classes.
"""

from __future__ import annotations

import io
import os
import runpy
import shutil
import stat
import sys
import tarfile
from collections.abc import Callable
from pathlib import Path

import pytest
from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT = load_script_module(
    REPO_ROOT / ".github" / "scripts" / "restore_unbundled_app.py"
)

BINARY = "target/release/esphome-desktop.exe"


def _file(
    name: str, data: bytes = b"x", mode: int = 0o644
) -> tuple[tarfile.TarInfo, bytes]:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    return info, data


def _dir(name: str) -> tuple[tarfile.TarInfo, bytes]:
    info = tarfile.TarInfo(name)
    info.type = tarfile.DIRTYPE
    info.mode = 0o755
    return info, b""


def _link(
    name: str, target: str, kind: bytes = tarfile.SYMTYPE
) -> tuple[tarfile.TarInfo, bytes]:
    info = tarfile.TarInfo(name)
    info.type = kind
    info.linkname = target
    return info, b""


def _write_tar(path: Path, members: list[tuple[tarfile.TarInfo, bytes]]) -> Path:
    with tarfile.open(path, "w") as archive:
        for info, data in members:
            archive.addfile(info, io.BytesIO(data) if info.isfile() else None)
    return path


def _good_members() -> list[tuple[tarfile.TarInfo, bytes]]:
    return [
        _file(BINARY, b"app", 0o755),
        _dir("python"),
        _dir("python/bin"),
        _file("python/bin/python3.14", b"py", 0o755),
        _link("python/bin/python3", "python3.14"),
        _file("./python/lib/site.py", b"site"),
        _link("python/lib/hard.py", "./python/lib/site.py", tarfile.LNKTYPE),
        _dir("git/"),
        _dir("ccache"),
    ]


def _members(members: list[tuple[tarfile.TarInfo, bytes]]) -> list[tarfile.TarInfo]:
    return [info for info, _ in members]


def test_good_archive_restores_with_modes_and_links(tmp_path: Path) -> None:
    src = tmp_path / "in"
    src.mkdir()
    _write_tar(src / "intermediate.tar", _good_members())
    dest = tmp_path / "src-tauri"
    dest.mkdir()
    assert (
        SCRIPT.main(
            ["--tarball-dir", str(src), "--binary", BINARY, "--dest", str(dest)]
        )
        == 0
    )
    assert (dest / BINARY).read_bytes() == b"app"
    assert (dest / "python/lib/hard.py").read_bytes() == b"site"
    assert (dest / "git").is_dir() and (dest / "ccache").is_dir()
    if os.name != "nt":
        assert os.readlink(dest / "python/bin/python3") == "python3.14"
        assert (dest / BINARY).stat().st_mode & stat.S_IXUSR


def test_setuid_bits_are_stripped(tmp_path: Path) -> None:
    tar = _write_tar(
        tmp_path / "a.tar",
        [*_good_members(), _file("python/bin/suid", b"s", 0o4755)],
    )
    dest = tmp_path / "out"
    SCRIPT.restore(tar, BINARY, dest)
    assert not (dest / "python/bin/suid").stat().st_mode & stat.S_ISUID


@pytest.mark.parametrize(
    "extra",
    [
        _file("tauri.windows-sign.conf.json", b"{}"),
        _file(".cargo/config.toml"),
        _file("target/release/other.exe"),
        _file("/etc/passwd"),
        _file("python/../tauri.conf.json"),
        _file("python//x"),
        _file("python\\..\\x"),
        _file("pythonic/x"),
        _link("python/escape", "../../build-scripts"),
        _link("python/abs", "/home/runner/.azure/msal_token_cache.json"),
        _link("python/cross", "../git/cmd"),
        _link("python/empty", ""),
        _link("python/hard", "tauri.conf.json", tarfile.LNKTYPE),
        _link("python/hard-dir", "python/bin", tarfile.LNKTYPE),
        _link(BINARY, "python/bin/python3"),
        _file("python/bin/python3/inside"),
    ],
    ids=lambda m: m[0].name,
)
def test_unexpected_or_escaping_members_are_rejected(
    extra: tuple[tarfile.TarInfo, bytes],
) -> None:
    with pytest.raises(SCRIPT.UnsafeArchiveError):
        SCRIPT.check_members(_members([*_good_members(), extra]), BINARY)


def test_binary_may_not_be_a_link() -> None:
    members = [_link(BINARY, "python/bin/python3"), *_good_members()[1:]]
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="must be a regular file"):
        SCRIPT.check_members(_members(members), BINARY)


def test_hard_link_into_another_tree_is_rejected() -> None:
    members = [
        *_good_members(),
        _file("git/cmd/git.exe"),
        _link("python/git.exe", "git/cmd/git.exe", tarfile.LNKTYPE),
    ]
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="points outside python/"):
        SCRIPT.check_members(_members(members), BINARY)


# Each link below is lexically inside python/, so a check that only
# normalises its own target passes it. Followed through the archive's other
# links, as the filesystem will, each one lands outside.
@pytest.mark.parametrize(
    "chain",
    [
        # The reported bypass: `d` really points at src-tauri/, so the config
        # file "inside" it overwrites src-tauri/tauri.windows-sign.conf.json.
        [
            _dir("python/a"),
            _dir("python/a/b"),
            _link("python/a/b/c", "../.."),
            _link("python/a/b/c/d", ".."),
            _file("python/a/b/c/d/tauri.windows-sign.conf.json", b"{}"),
        ],
        # No member is written through a link, but the second link resolves
        # through the first to src-tauri/, so the bundler would copy it in.
        [
            _dir("python/a"),
            _dir("python/a/b"),
            _link("python/a/b/c", "../.."),
            _link("python/x", "a/b/c/.."),
        ],
        # A link to a link that climbs out.
        [_link("python/up", ".."), _link("python/via", "up/tauri.conf.json")],
    ],
    ids=["write-through-chain", "resolve-through-chain", "link-to-escaping-link"],
)
def test_symlink_chains_that_escape_are_rejected(
    chain: list[tuple[tarfile.TarInfo, bytes]],
) -> None:
    with pytest.raises(SCRIPT.UnsafeArchiveError):
        SCRIPT.check_members(_members([*_good_members(), *chain]), BINARY)


def test_write_through_chain_leaves_the_config_untouched(tmp_path: Path) -> None:
    # The exact archive from the report, run through the real restore path.
    dest = tmp_path / "src-tauri"
    dest.mkdir()
    config = dest / "tauri.windows-sign.conf.json"
    config.write_text("original", encoding="utf-8")
    tar = _write_tar(
        tmp_path / "a.tar",
        [
            *_good_members(),
            _dir("python/a"),
            _dir("python/a/b"),
            _link("python/a/b/c", "../.."),
            _link("python/a/b/c/d", ".."),
            _file("python/a/b/c/d/tauri.windows-sign.conf.json", b"{}"),
        ],
    )
    with pytest.raises(SCRIPT.UnsafeArchiveError):
        SCRIPT.restore(tar, BINARY, dest)
    assert config.read_text(encoding="utf-8") == "original"


def test_symlink_chain_inside_its_tree_is_allowed() -> None:
    members = [
        *_good_members(),
        _dir("python/lib/pkg"),
        _link("python/lib/current", "pkg"),
        _link("python/bin/site", "./../lib/current/../site.py"),
    ]
    SCRIPT.check_members(_members(members), BINARY)


def test_symlink_loop_is_rejected() -> None:
    members = [
        *_good_members(),
        _link("python/loop-a", "loop-b"),
        _link("python/loop-b", "loop-a"),
    ]
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="symlink loop"):
        SCRIPT.check_members(_members(members), BINARY)


@pytest.mark.parametrize(
    "chain",
    [
        # The case-folding bypass: on APFS/NTFS `L` is the link `l`, so `x`
        # really points at src-tauri/ and `X/...` is written through it.
        [
            _link("python/l", "."),
            _link("python/x", "L/.."),
            _file("python/X/tauri.macos.conf.json", b"{}"),
        ],
        # The same links without the write: `x` would still resolve to
        # src-tauri/, so the bundler could copy files from above the tree.
        [_link("python/l", "."), _link("python/x", "L/..")],
        # Unicode normalisation: APFS treats the NFD and NFC spellings of a
        # name as one entry.
        [_link("python/é", ".."), _link("python/y", "é/tauri.conf.json")],
    ],
    ids=["case-fold-write-through", "case-fold-resolve", "unicode-normalisation"],
)
def test_names_the_filesystem_folds_together_are_one_name(
    chain: list[tuple[tarfile.TarInfo, bytes]],
) -> None:
    with pytest.raises(SCRIPT.UnsafeArchiveError):
        SCRIPT.check_members(_members([*_good_members(), *chain]), BINARY)


def test_names_differing_only_in_case_are_duplicates() -> None:
    members = [*_good_members(), _file("python/LIB/SITE.PY", b"other")]
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="duplicate member names"):
        SCRIPT.check_members(_members(members), BINARY)


def _staging(tmp_path: Path) -> Path:
    """An unpacked tree as check_extracted sees it, built directly on disk."""
    staging = tmp_path / "staging"
    (staging / "target/release").mkdir(parents=True)
    (staging / BINARY).write_bytes(b"app")
    (staging / "python/bin").mkdir(parents=True)
    (staging / "python/bin/python3.14").write_bytes(b"py")
    (staging / "git").mkdir()
    return staging


def test_extracted_tree_check_accepts_a_good_tree(tmp_path: Path) -> None:
    staging = _staging(tmp_path)
    if os.name != "nt":
        (staging / "python/bin/python3").symlink_to("python3.14")
    SCRIPT.check_extracted(staging, BINARY)


@pytest.mark.skipif(os.name == "nt", reason="symlinks need privileges on Windows")
@pytest.mark.parametrize(
    ("make", "problem"),
    [
        # Whatever the member checks missed, the real filesystem catches.
        (lambda s: (s / "python/up").symlink_to(".."), "resolves outside python/"),
        (
            lambda s: (s / "python/bin/deep").symlink_to("../../git"),
            "resolves outside python/",
        ),
        (lambda s: (s / "ccache").symlink_to("python"), "ccache/ is not a directory"),
        (lambda s: (s / "git").rmdir() or (s / "git").write_bytes(b""), "git/ is not"),
        (lambda s: (s / "extra").mkdir(), "unexpected entry after unpacking: extra"),
        (
            lambda s: (s / BINARY).unlink() or (s / BINARY).symlink_to("../../git"),
            "is not a regular file",
        ),
        (
            lambda s: (
                (s / "python/release").mkdir(),
                (s / "python/release/esphome-desktop.exe").write_bytes(b"x"),
                shutil.rmtree(s / "target"),
                (s / "target").mkdir(),
                (s / "target/release").symlink_to("../python/release"),
            ),
            "resolves elsewhere",
        ),
    ],
    ids=[
        "link-above-tree",
        "link-into-other-tree",
        "tree-is-symlink",
        "tree-is-file",
        "unexpected-top-level",
        "binary-is-symlink",
        "binary-parent-is-symlink",
    ],
)
def test_extracted_tree_check_rejects_escapes(
    tmp_path: Path, make: Callable[[Path], object], problem: str
) -> None:
    staging = _staging(tmp_path)
    make(staging)
    with pytest.raises(SCRIPT.UnsafeArchiveError, match=problem):
        SCRIPT.check_extracted(staging, BINARY)


@pytest.mark.parametrize("existing", ["python", BINARY])
def test_restore_refuses_to_overwrite_existing_paths(
    tmp_path: Path, existing: str
) -> None:
    tar = _write_tar(tmp_path / "a.tar", _good_members())
    dest = tmp_path / "src-tauri"
    (dest / existing).parent.mkdir(parents=True, exist_ok=True)
    (dest / existing).write_bytes(b"already here")
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="already exists"):
        SCRIPT.restore(tar, BINARY, dest)
    assert (dest / existing).read_bytes() == b"already here"


def test_restore_skips_a_tree_the_archive_lacks(tmp_path: Path) -> None:
    members = [m for m in _good_members() if not m[0].name.startswith("ccache")]
    tar = _write_tar(tmp_path / "a.tar", members)
    dest = tmp_path / "src-tauri"
    SCRIPT.restore(tar, BINARY, dest)
    assert (dest / "python").is_dir()
    assert not (dest / "ccache").exists()


def test_main_stages_under_runner_temp(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    src = tmp_path / "in"
    src.mkdir()
    _write_tar(src / "intermediate.tar", _good_members())
    runner_temp = tmp_path / "runner-temp"
    runner_temp.mkdir()
    monkeypatch.setenv("RUNNER_TEMP", str(runner_temp))
    dest = tmp_path / "src-tauri"
    args = ["--tarball-dir", str(src), "--binary", BINARY, "--dest", str(dest)]
    assert SCRIPT.main(args) == 0
    assert (dest / BINARY).read_bytes() == b"app"
    # The staging directory is cleaned up once its contents are moved out.
    assert list(runner_temp.iterdir()) == []


def test_duplicate_member_names_are_rejected() -> None:
    members = [*_good_members(), _link("python/lib/site.py", "../../x")]
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="duplicate member names"):
        SCRIPT.check_members(_members(members), BINARY)


def test_device_and_fifo_members_are_rejected() -> None:
    fifo = tarfile.TarInfo("python/fifo")
    fifo.type = tarfile.FIFOTYPE
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="unsupported member type"):
        SCRIPT.check_members([*_members(_good_members()), fifo], BINARY)


def test_binary_may_not_be_a_directory() -> None:
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="unsupported member type"):
        SCRIPT.check_members([_dir(BINARY)[0], *_members(_good_members()[1:])], BINARY)


def test_missing_binary_is_rejected() -> None:
    with pytest.raises(SCRIPT.UnsafeArchiveError, match="is missing"):
        SCRIPT.check_members(_members(_good_members()[1:]), BINARY)


def test_rejection_writes_nothing(tmp_path: Path) -> None:
    tar = _write_tar(
        tmp_path / "a.tar", [*_good_members(), _file("tauri.conf.json", b"{}")]
    )
    dest = tmp_path / "out"
    dest.mkdir()
    with pytest.raises(SCRIPT.UnsafeArchiveError):
        SCRIPT.restore(tar, BINARY, dest)
    assert list(dest.iterdir()) == []


@pytest.mark.parametrize("count", [0, 2])
def test_exactly_one_tarball_is_required(
    tmp_path: Path, count: int, capsys: pytest.CaptureFixture[str]
) -> None:
    for i in range(count):
        _write_tar(tmp_path / f"{i}.tar", _good_members())
    assert SCRIPT.main(["--tarball-dir", str(tmp_path), "--binary", BINARY]) == 1
    assert f"found {count}" in capsys.readouterr().err


def test_corrupt_tarball_is_reported(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    (tmp_path / "bad.tar").write_bytes(b"not a tar" * 100)
    dest = tmp_path / "out"
    args = ["--tarball-dir", str(tmp_path), "--binary", BINARY, "--dest", str(dest)]
    assert SCRIPT.main(args) == 1
    assert "::error::refusing to restore" in capsys.readouterr().err


def test_entry_point_exits_with_mains_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    script = REPO_ROOT / ".github" / "scripts" / "restore_unbundled_app.py"
    argv = [str(script), "--tarball-dir", str(tmp_path), "--binary", BINARY]
    monkeypatch.setattr(sys, "argv", argv)
    with pytest.raises(SystemExit) as exc:
        runpy.run_path(str(script), run_name="__main__")
    assert exc.value.code == 1
