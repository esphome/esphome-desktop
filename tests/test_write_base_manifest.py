"""Tests for build-scripts/write_base_manifest.py (#330).

The manifest is the sole definition of "not ours to delete": the app's package
reset removes every site-packages entry the manifest does not name. A generator
bug therefore deletes the interpreter's own pip, so these run the real script
against a real interpreter rather than asserting on strings.

The format is the other half of a contract with `parse_base_manifest` in
src-tauri/src/platform/mod.rs, which has its own tests pinning the same grammar.
"""

from __future__ import annotations

import subprocess
import sys
import venv
from pathlib import Path

import pytest
from script_loader import REPO_ROOT, load_script_module

SCRIPT_PATH = REPO_ROOT / "build-scripts" / "write_base_manifest.py"

maint = load_script_module(SCRIPT_PATH)


@pytest.fixture(scope="module")
def real_tree(tmp_path_factory: pytest.TempPathFactory) -> Path:
    """A real Python tree with pip in it, and nothing else installed.

    A venv has the same shape the manifest cares about: an interpreter, a
    sysconfig-resolvable site-packages and scripts dir, and pip. Building one is
    the cheapest way to run the generator against a genuine interpreter on every
    platform we ship.
    """
    root = tmp_path_factory.mktemp("base-manifest-tree")
    venv.create(root, with_pip=True, clear=True)
    return root


def tree_python(root: Path) -> Path:
    return root / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python3")


def generate(root: Path) -> str:
    """Run the generator the way prepare_bundle.sh does: with the tree's own
    interpreter, which is what makes sysconfig report that tree."""
    result = subprocess.run(
        [str(tree_python(root)), str(SCRIPT_PATH), str(root)],
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout


def parse(manifest: str) -> tuple[list[str], list[str]]:
    """Split a manifest into its sweep and keep paths, ignoring comments."""
    sweep, keep = [], []
    for line in manifest.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        verb, path = line.split(" ", 1)
        {"sweep": sweep, "keep": keep}[verb].append(path)
    return sweep, keep


def test_records_pip_and_the_interpreter(real_tree: Path) -> None:
    # pip is the one thing that must never be deleted: the reset reinstalls with
    # it, so losing it makes the tree unrepairable.
    _, keep = parse(generate(real_tree))
    names = {path.rsplit("/", 1)[-1] for path in keep}
    assert "pip" in names
    assert any(n.startswith("pip-") and n.endswith(".dist-info") for n in names)


def test_sweeps_site_packages_and_the_scripts_dir(real_tree: Path) -> None:
    # Both must be swept, or pip-installed entry points (`esphome`, `esptool`)
    # survive a reset as orphans.
    sweep, _ = parse(generate(real_tree))
    assert len(sweep) == 2, sweep
    assert any(s.endswith("site-packages") for s in sweep), sweep
    assert any(s.rsplit("/", 1)[-1] in {"bin", "Scripts"} for s in sweep), sweep


def test_every_path_is_relative_and_posix(real_tree: Path) -> None:
    # The Rust side rejects absolute paths and `..` outright, and resolves these
    # against the tree root at runtime, which is a different directory to the one
    # they were generated in.
    sweep, keep = parse(generate(real_tree))
    for path in sweep + keep:
        assert not path.startswith("/"), path
        assert ".." not in path.split("/"), path
        assert "\\" not in path, path
        assert ":" not in path, path


def test_keep_entries_live_under_a_swept_dir(real_tree: Path) -> None:
    # A keep that is not inside a swept dir can never match anything, so it would
    # silently protect nothing.
    sweep, keep = parse(generate(real_tree))
    for path in keep:
        assert any(path.startswith(f"{s}/") for s in sweep), path


def test_generated_manifest_is_stable(real_tree: Path) -> None:
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
