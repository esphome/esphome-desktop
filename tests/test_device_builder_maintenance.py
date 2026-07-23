#!/usr/bin/env python3
"""Tests for src-tauri/scripts/device_builder_maintenance.py.

The bundled Python can accumulate duplicate esphome-device-builder dist-info
dirs, which makes importlib.metadata return None or the wrong version and loops
the in-app updater forever (#190). This suite pins the version ranking, the
robust detection, and the dedup so a regression cannot reintroduce the loop or,
worse, delete the wrong dist-info directory.

The detection and dedup helpers are exercised against real importlib.metadata
Distribution objects: fixtures fabricate dist-info dirs in a tmp path and load
them with ``distributions(path=[...])``, so the tests cover the same code paths
the bundled interpreter runs.

pytest suite (maintainer-requested framework, fully typed, no classes).
"""

from __future__ import annotations

from importlib.metadata import Distribution, distributions
from pathlib import Path

from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / "src-tauri" / "scripts" / "device_builder_maintenance.py"


maint = load_script_module(SCRIPT_PATH)


def _make_dist_info(
    site: Path,
    package: str,
    version: str | None,
    *,
    with_version: bool = True,
    with_name: bool = True,
) -> Path:
    """Create a *.dist-info dir for ``package`` and return its path."""
    dist_info = site / f"{package.replace('-', '_')}-{version}.dist-info"
    dist_info.mkdir(parents=True)
    lines = ["Metadata-Version: 2.1"]
    if with_name:
        lines.append(f"Name: {package}")
    if with_version and version is not None:
        lines.append(f"Version: {version}")
    (dist_info / "METADATA").write_text("\n".join(lines) + "\n")
    return dist_info


def _dists(site: Path) -> list[Distribution]:
    return list(distributions(path=[str(site)]))


# --------------------------------------------------------------------------- #
# vkey: self-contained version ranking (no packaging dependency).
# --------------------------------------------------------------------------- #


def test_vkey_release_outranks_prerelease() -> None:
    assert maint.vkey("1.0.10") > maint.vkey("1.0.10b1")
    assert maint.vkey("1.0.10") > maint.vkey("1.0.9")
    assert maint.vkey("1.0.10b1") > maint.vkey("1.0.1")


def test_vkey_prerelease_precedence() -> None:
    # dev < a < b < rc < release
    assert maint.vkey("1.0.0dev1") < maint.vkey("1.0.0a1")
    assert maint.vkey("1.0.0a1") < maint.vkey("1.0.0b1")
    assert maint.vkey("1.0.0b1") < maint.vkey("1.0.0rc1")
    assert maint.vkey("1.0.0rc1") < maint.vkey("1.0.0")


def test_vkey_spelled_out_tag_keeps_serial() -> None:
    # Longest-first alternation: "alpha2" must keep its serial, not collapse to
    # the leading "a" and drop the "2".
    assert maint.vkey("1.0.0alpha2") > maint.vkey("1.0.0alpha1")
    assert maint.vkey("1.0.0beta2") > maint.vkey("1.0.0beta1")
    assert maint.vkey("1.0.0preview2") > maint.vkey("1.0.0preview1")
    # Spelled-out and short forms rank equally by tag.
    assert maint.vkey("1.0.0alpha1") == maint.vkey("1.0.0a1")


def test_vkey_unparseable_sorts_lowest() -> None:
    lowest = ((), 0, 0)
    assert maint.vkey(None) == lowest
    assert maint.vkey("") == lowest
    assert maint.vkey("None") == lowest
    assert maint.vkey("garbage") == lowest
    assert maint.vkey("1.0.0") > maint.vkey("None")


# --------------------------------------------------------------------------- #
# detect_version: robust to the duplicate dist-info pileup.
# --------------------------------------------------------------------------- #


def test_detect_version_picks_highest_among_duplicates(tmp_path: Path) -> None:
    for version in ("1.0.1", "1.0.9", "1.0.10", "1.0.10b1"):
        _make_dist_info(tmp_path, "esphome-device-builder", version)
    assert maint.detect_version(_dists(tmp_path)) == "1.0.10"


def test_detect_version_ignores_duplicate_without_version(tmp_path: Path) -> None:
    # A duplicate whose METADATA lost its Version header (the orphaned None case)
    # must not mask the real highest version.
    _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9", with_version=False)
    assert maint.detect_version(_dists(tmp_path)) == "1.0.10"


def test_detect_version_returns_none_when_absent(tmp_path: Path) -> None:
    _make_dist_info(tmp_path, "some-other-package", "1.2.3")
    assert maint.detect_version(_dists(tmp_path)) is None


# --------------------------------------------------------------------------- #
# dedupe_dist_info: heal the pileup, keep the right one.
# --------------------------------------------------------------------------- #


def test_dedupe_keeps_highest_and_removes_rest(tmp_path: Path) -> None:
    paths = {
        version: _make_dist_info(tmp_path, "esphome-device-builder", version)
        for version in ("1.0.1", "1.0.9", "1.0.10", "1.0.10b1")
    }
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 3
    assert paths["1.0.10"].is_dir()
    for version in ("1.0.1", "1.0.9", "1.0.10b1"):
        assert not paths[version].exists()
    # importlib now resolves a single, correct version.
    assert maint.detect_version(_dists(tmp_path)) == "1.0.10"


def test_dedupe_never_deletes_an_unparseable_duplicate(tmp_path: Path) -> None:
    # A dist-info whose version can't be parsed might itself be the real install,
    # so the destructive prune must keep it rather than trust the lowest-sort
    # sentinel. detect_version still reports the real version regardless.
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    broken = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.9", with_version=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 0
    assert keep.is_dir()
    assert broken.is_dir()
    assert maint.detect_version(_dists(tmp_path)) == "1.0.10"


def test_dedupe_prunes_parseable_but_spares_unparseable_sibling(tmp_path: Path) -> None:
    # A parseable lower version is still pruned even when an unparseable sibling
    # is present; only the unrankable entry is spared.
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    stale = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9")
    broken = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.8", with_version=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 1
    assert keep.is_dir()
    assert not stale.exists()
    assert broken.is_dir()


def test_dedupe_skips_group_with_no_parseable_version(tmp_path: Path) -> None:
    # If nothing in the group parses, we can't pick a winner; leave it all alone.
    a = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9", with_version=False)
    b = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.10", with_version=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 0
    assert a.is_dir()
    assert b.is_dir()


def test_dedupe_leaves_single_install_untouched(tmp_path: Path) -> None:
    only = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 0
    assert only.is_dir()


def test_dedupe_groups_frontend_independently(tmp_path: Path) -> None:
    main_keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9")
    fe_keep = _make_dist_info(tmp_path, "esphome-device-builder-frontend", "0.1.170")
    _make_dist_info(tmp_path, "esphome-device-builder-frontend", "0.1.158")
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 2
    assert main_keep.is_dir()
    assert fe_keep.is_dir()


# --------------------------------------------------------------------------- #
# dedupe_dist_info(targets=None): the post-copy self-clean (#389).
# --------------------------------------------------------------------------- #


def test_dedupe_default_scope_ignores_non_target_duplicates(tmp_path: Path) -> None:
    # The #190 heal must stay scoped to the device-builder packages: a plain
    # esphome pileup is the copy path's job (dedupe-all), not the update
    # check's.
    old = _make_dist_info(tmp_path, "esphome", "2026.7.0")
    new = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    assert maint.dedupe_dist_info(_dists(tmp_path)) == 0
    assert old.is_dir()
    assert new.is_dir()


def test_dedupe_all_prunes_any_package(tmp_path: Path) -> None:
    # The live #389 shape: the installer overlays the bundle without deleting
    # the previous release's files, stranding its dist-info next to the new
    # one for several packages at once.
    esphome_old = _make_dist_info(tmp_path, "esphome", "2026.7.0")
    esphome_new = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    aioesp_old = _make_dist_info(tmp_path, "aioesphomeapi", "45.6.0")
    aioesp_new = _make_dist_info(tmp_path, "aioesphomeapi", "45.6.2")
    single = _make_dist_info(tmp_path, "bleak", "2.1.1")
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == 2
    assert esphome_new.is_dir()
    assert aioesp_new.is_dir()
    assert single.is_dir()
    assert not esphome_old.exists()
    assert not aioesp_old.exists()


def test_dedupe_all_never_groups_nameless_dist_infos(tmp_path: Path) -> None:
    # Two unrelated dist-infos whose METADATA lost its Name header normalize
    # to ""; grouping them would prune one as a "duplicate" of the other even
    # though they belong to different packages. Both must survive, and a
    # healthy pair must still dedupe normally alongside them.
    orphan_a = _make_dist_info(tmp_path, "pkg-a", "1.0.0", with_name=False)
    orphan_b = _make_dist_info(tmp_path, "pkg-b", "2.0.0", with_name=False)
    keep = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    stale = _make_dist_info(tmp_path, "esphome", "2026.7.0")
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == 1
    assert orphan_a.is_dir()
    assert orphan_b.is_dir()
    assert keep.is_dir()
    assert not stale.exists()


def test_dedupe_all_keeps_safety_guards(tmp_path: Path) -> None:
    # The guard behavior must survive the scope widening: an all-unparseable
    # group is left whole, and an unparseable sibling is never deleted on the
    # strength of the lowest-sort sentinel.
    amb_a = _make_dist_info(tmp_path, "aioesphomeapi", "45.6.0", with_version=False)
    amb_b = _make_dist_info(tmp_path, "aioesphomeapi", "45.6.2", with_version=False)
    keep = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    broken = _make_dist_info(tmp_path, "esphome", "2026.7.0", with_version=False)
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == 0
    for path in (amb_a, amb_b, keep, broken):
        assert path.is_dir()
