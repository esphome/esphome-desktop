"""Tests for build-scripts/write_base_manifest.py (#330).

The manifest is the sole definition of "not ours to delete": the app's package
reset removes every site-packages entry the manifest does not name. A generator
bug therefore deletes the interpreter's own pip, so these run the real script
against a real interpreter rather than asserting on strings.

The format is the other half of a contract with `parse_base_manifest` in
src-tauri/src/platform/mod.rs, which has its own tests pinning the same grammar.
"""

from __future__ import annotations

import os
import subprocess
import sysconfig
import venv
from pathlib import Path

import pytest
from script_loader import REPO_ROOT, load_script_module

SCRIPT_PATH = REPO_ROOT / "build-scripts" / "write_base_manifest.py"

maint = load_script_module(SCRIPT_PATH)


@pytest.fixture(scope="module")
def real_tree(tmp_path_factory: pytest.TempPathFactory) -> tuple[Path, Path]:
    """A real Python tree with pip in it and nothing else installed.

    Returns (root, interpreter).

    A venv has the same shape the manifest cares about: an interpreter, a
    sysconfig-resolvable site-packages and scripts dir, and pip. Building one is
    the cheapest way to run the generator against a genuine interpreter on every
    platform we ship.

    The interpreter path comes from venv's own context rather than being spelled
    out, because which of `python`, `python3` and `python3.X` a venv creates
    varies by platform and distro; `env_exe` is the one it guarantees.
    """
    root = tmp_path_factory.mktemp("base-manifest-tree")
    venv.create(root, with_pip=True, clear=True)
    # A non-clearing builder purely to read back the context: `ensure_directories`
    # on a `clear=True` builder would wipe the venv that was just built.
    context = venv.EnvBuilder().ensure_directories(root)
    interpreter = Path(context.env_exe)
    assert interpreter.is_file(), f"venv created no interpreter at {interpreter}"
    return root, interpreter


def generate(tree: tuple[Path, Path]) -> str:
    """Run the generator the way prepare_bundle.sh does: with the tree's own
    interpreter, which is what makes sysconfig report that tree."""
    root, interpreter = tree
    result = subprocess.run(
        [str(interpreter), str(SCRIPT_PATH), str(root)],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


@pytest.fixture(scope="module")
def manifest(real_tree: tuple[Path, Path]) -> tuple[list[str], list[str]]:
    """The generated manifest, parsed into (sweep, keep).

    Module-scoped alongside `real_tree`: the tree never changes, so the
    generator would otherwise be re-run per test to produce identical bytes.
    """
    return parse(generate(real_tree))


def parse(manifest: str) -> tuple[list[str], list[str]]:
    """Split a manifest into its sweep and keep paths, ignoring comments.

    Splits on any whitespace, mirroring the Rust parser's
    `split_once(char::is_whitespace)`. These tests exist to pin the format both
    sides agree on, so a test-side parser that is stricter than production would
    fail on manifests the real reader accepts.
    """
    sweep, keep = [], []
    for line in manifest.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        verb, path = line.split(maxsplit=1)
        {"sweep": sweep, "keep": keep}[verb].append(path)
    return sweep, keep


def test_records_pip_and_the_interpreter(manifest: tuple[list[str], list[str]]) -> None:
    # pip is the one thing that must never be deleted: the reset reinstalls with
    # it, so losing it makes the tree unrepairable.
    _, keep = manifest
    names = {path.rsplit("/", 1)[-1] for path in keep}
    assert "pip" in names
    assert any(n.startswith("pip-") and n.endswith(".dist-info") for n in names)


def test_sweeps_site_packages_and_the_scripts_dir(
    manifest: tuple[list[str], list[str]],
) -> None:
    # Both must be swept, or pip-installed entry points (`esphome`, `esptool`)
    # survive a reset as orphans.
    sweep, _ = manifest
    assert len(sweep) == 2, sweep
    assert any(s.endswith("site-packages") for s in sweep), sweep
    assert any(s.rsplit("/", 1)[-1] in {"bin", "Scripts"} for s in sweep), sweep


def test_every_path_is_relative_and_posix(
    manifest: tuple[list[str], list[str]],
) -> None:
    # The Rust side rejects absolute paths and `..` outright, and resolves these
    # against the tree root at runtime, which is a different directory to the one
    # they were generated in.
    sweep, keep = manifest
    for path in sweep + keep:
        assert not path.startswith("/"), path
        assert ".." not in path.split("/"), path
        assert "\\" not in path, path
        assert ":" not in path, path


def test_keep_entries_live_under_a_swept_dir(
    manifest: tuple[list[str], list[str]],
) -> None:
    # A keep that is not inside a swept dir can never match anything, so it would
    # silently protect nothing.
    sweep, keep = manifest
    for path in keep:
        assert any(path.startswith(f"{s}/") for s in sweep), path


def test_generated_manifest_is_stable(real_tree: tuple[Path, Path]) -> None:
    # The entries are sorted, so a rebuild of the same tree produces the same
    # file rather than a directory-order-dependent one.
    assert generate(real_tree) == generate(real_tree)


def test_rejects_a_root_that_is_not_the_running_tree(tmp_path: Path) -> None:
    # The paths come from the *running* interpreter's sysconfig, so a mismatched
    # root would record entries relative to the wrong tree. Fail rather than emit
    # a manifest that aims the reset at the wrong directory.
    with pytest.raises(SystemExit):
        maint.build_manifest(tmp_path)


def test_usage_error_without_a_root() -> None:
    assert maint.main([]) == 2


def running_tree_root() -> Path:
    """The root of the interpreter running the tests.

    `build_manifest` reads the *running* interpreter's sysconfig, so it can only
    be called in-process against that interpreter's own tree — handing it the
    venv's root trips the "outside the Python tree" guard instead of whatever the
    test meant to exercise. Derived as the common ancestor of the paths it
    records, rather than `sys.prefix`, which symlinks can put somewhere else, and
    read from the generator's own `SWEPT_PATHS` so adding a swept dir cannot
    leave this answering for the wrong subset of the tree.
    """
    swept = [Path(sysconfig.get_path(key)).resolve() for key in maint.SWEPT_PATHS]
    return Path(os.path.commonpath(swept))


def test_rejects_a_swept_path_that_is_not_a_directory(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A swept path that is not a directory would be recorded with nothing to keep
    # inside it, and the reset would then clean it out entirely -- which for
    # site-packages means deleting pip, the one thing no repair recovers from.
    monkeypatch.setattr(maint.Path, "is_dir", lambda self: False)
    with pytest.raises(SystemExit, match="not a directory"):
        maint.build_manifest(running_tree_root())


def test_refuses_a_manifest_that_does_not_name_pip(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # The repair reinstalls with pip, so a manifest that fails to spare it is
    # worse than useless. Fail the build that produced it, rather than the user's
    # repair months later.
    root = running_tree_root()
    if "keep " not in maint.build_manifest(root):
        pytest.skip("the running interpreter records no packages to keep")

    real_iterdir = maint.Path.iterdir
    monkeypatch.setattr(
        maint.Path,
        "iterdir",
        lambda self: (p for p in real_iterdir(self) if p.name != "pip"),
    )
    with pytest.raises(SystemExit, match="pip"):
        maint.build_manifest(root)
