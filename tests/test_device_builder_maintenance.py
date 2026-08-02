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

import os
from importlib.metadata import Distribution, PathDistribution, distributions
from pathlib import Path
from types import SimpleNamespace

import pytest
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
    with_record: bool = True,
    with_metadata: bool = True,
) -> Path:
    """Create a *.dist-info dir for ``package`` and return its path.

    ``with_record=False`` and ``with_metadata=False`` fabricate the torn shapes
    the installer overlay leaves behind; the default is a pip-manageable entry
    (a completed pip install always writes RECORD).
    """
    dist_info = site / f"{package.replace('-', '_')}-{version}.dist-info"
    dist_info.mkdir(parents=True)
    if with_metadata:
        lines = ["Metadata-Version: 2.1"]
        if with_name:
            lines.append(f"Name: {package}")
        if with_version and version is not None:
            lines.append(f"Version: {version}")
        (dist_info / "METADATA").write_text("\n".join(lines) + "\n")
    if with_record:
        (dist_info / "RECORD").write_text("")
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
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (3, 0)
    assert paths["1.0.10"].is_dir()
    for version in ("1.0.1", "1.0.9", "1.0.10b1"):
        assert not paths[version].exists()
    # importlib now resolves a single, correct version.
    assert maint.detect_version(_dists(tmp_path)) == "1.0.10"


def test_dedupe_never_deletes_an_unparseable_duplicate(tmp_path: Path) -> None:
    # A dist-info whose version can't be parsed but that pip can still manage
    # (it has a RECORD) might itself be the real install, so the destructive
    # prune must keep it rather than trust the lowest-sort sentinel.
    # detect_version still reports the real version regardless.
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    broken = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.9", with_version=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
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
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (1, 0)
    assert keep.is_dir()
    assert not stale.exists()
    assert broken.is_dir()


def test_dedupe_skips_group_with_no_parseable_version(tmp_path: Path) -> None:
    # If nothing in the group parses, we can't pick a winner; leave it all alone.
    a = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9", with_version=False)
    b = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.10", with_version=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
    assert a.is_dir()
    assert b.is_dir()


def test_dedupe_leaves_single_install_untouched(tmp_path: Path) -> None:
    only = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
    assert only.is_dir()


def test_dedupe_groups_frontend_independently(tmp_path: Path) -> None:
    main_keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9")
    fe_keep = _make_dist_info(tmp_path, "esphome-device-builder-frontend", "0.1.170")
    _make_dist_info(tmp_path, "esphome-device-builder-frontend", "0.1.158")
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (2, 0)
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
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
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
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == (2, 0)
    assert esphome_new.is_dir()
    assert aioesp_new.is_dir()
    assert single.is_dir()
    assert not esphome_old.exists()
    assert not aioesp_old.exists()


def test_dedupe_all_never_groups_nameless_dist_infos(tmp_path: Path) -> None:
    # Two unrelated dist-infos whose METADATA lost its Name header are
    # attributed by their directory names, so they land in separate groups
    # rather than being pruned as "duplicates" of one another. Both must
    # survive (each is the only entry for its package and both still have a
    # RECORD), and a healthy pair must still dedupe normally alongside them.
    orphan_a = _make_dist_info(tmp_path, "pkg-a", "1.0.0", with_name=False)
    orphan_b = _make_dist_info(tmp_path, "pkg-b", "2.0.0", with_name=False)
    keep = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    stale = _make_dist_info(tmp_path, "esphome", "2026.7.0")
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == (1, 0)
    assert orphan_a.is_dir()
    assert orphan_b.is_dir()
    assert keep.is_dir()
    assert not stale.exists()


def test_dedupe_all_keeps_safety_guards(tmp_path: Path) -> None:
    # The guard behavior must survive the scope widening: an all-unparseable
    # group of managed entries is left whole, and an unparseable sibling with
    # a RECORD is never deleted on the strength of the lowest-sort sentinel.
    amb_a = _make_dist_info(tmp_path, "aioesphomeapi", "45.6.0", with_version=False)
    amb_b = _make_dist_info(tmp_path, "aioesphomeapi", "45.6.2", with_version=False)
    keep = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    broken = _make_dist_info(tmp_path, "esphome", "2026.7.0", with_version=False)
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == (0, 0)
    for path in (amb_a, amb_b, keep, broken):
        assert path.is_dir()


# --------------------------------------------------------------------------- #
# RECORD-less dist-infos: pip can never uninstall one, so every upgrade aborts
# with uninstall-no-record-file ("Cannot uninstall <pkg> None", or with the
# version when METADATA survived) until it is removed.
# --------------------------------------------------------------------------- #


def test_dedupe_removes_record_less_frontend_beside_healthy(tmp_path: Path) -> None:
    # The reported Windows failure: the installer overlay leaves a torn
    # frontend dist-info (no METADATA, no RECORD) in the bundled tree, the
    # repair re-copy faithfully restores it, and the retried install hits the
    # same abort forever. The name comes from the directory alone, so the
    # prune must still attribute and remove it while keeping the healthy
    # install.
    keep = _make_dist_info(tmp_path, "esphome-device-builder-frontend", "0.1.172")
    dead = _make_dist_info(
        tmp_path,
        "esphome-device-builder-frontend",
        "0.1.150",
        with_metadata=False,
        with_record=False,
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (1, 0)
    assert keep.is_dir()
    assert not dead.exists()
    assert (keep / "RECORD").is_file()


def test_dedupe_removes_lone_record_less_entry(tmp_path: Path) -> None:
    # No healthy sibling to compare against: the entry is condemned on its own
    # evidence. A completed pip install always writes RECORD, so this cannot
    # be the real install, and pip aborts the upgrade as long as it exists.
    dead = _make_dist_info(
        tmp_path,
        "esphome-device-builder-frontend",
        "0.1.150",
        with_metadata=False,
        with_record=False,
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (1, 0)
    assert not dead.exists()


def test_dedupe_removes_dist_info_with_read_only_contents(tmp_path: Path) -> None:
    # On Windows a read-only file inside the dist-info fails a plain
    # shutil.rmtree, which would leave the abort loop in place with only a
    # "skip" line on stderr. The remover must clear the flag and retry (on
    # POSIX the delete succeeds either way, so this bites on the Windows CI
    # leg, where the original failure lived).
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    stale = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9")
    dead = _make_dist_info(
        tmp_path,
        "esphome-device-builder-frontend",
        "0.1.150",
        with_metadata=False,
        with_record=False,
    )
    for dist_info in (stale, dead):
        locked = dist_info / "locked.txt"
        locked.write_text("")
        locked.chmod(0o444)
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (2, 0)
    assert keep.is_dir()
    assert not stale.exists()
    assert not dead.exists()


def test_dedupe_removes_record_less_entry_with_healthy_metadata(tmp_path: Path) -> None:
    # The other torn shape: METADATA intact, RECORD gone. pip aborts the
    # upgrade identically (the message just carries the version instead of
    # None), so version readability must not shield the entry; the abort keys
    # off the missing RECORD alone.
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    torn = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.9", with_record=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (1, 0)
    assert keep.is_dir()
    assert not torn.exists()


def test_dedupe_counts_a_record_less_entry_it_cannot_remove(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A RECORD-less dir that survives the prune still aborts every install, so
    # it must be reported as a failure (the CLI exits non-zero on it), never
    # folded into "nothing to remove".
    dead = _make_dist_info(
        tmp_path,
        "esphome-device-builder-frontend",
        "0.1.150",
        with_metadata=False,
        with_record=False,
    )

    def refuse(_path: Path) -> None:
        raise OSError("locked")

    monkeypatch.setattr(maint, "_rmtree", refuse)
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 1)
    assert dead.is_dir()


def _fail_iterdir_for(monkeypatch: pytest.MonkeyPatch, broken: Path) -> None:
    """Make ``Path.iterdir`` raise for ``broken`` and pass through otherwise."""
    real_iterdir = Path.iterdir

    def failing_iterdir(self: Path) -> object:
        if self == broken:
            raise OSError("transient read failure")
        return real_iterdir(self)

    monkeypatch.setattr(Path, "iterdir", failing_iterdir)


def test_dedupe_counts_an_unlistable_dist_info(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A directory that cannot be listed decides nothing: a transient read
    # failure must not turn "RECORD not seen" into "RECORD absent" and delete
    # what might be a real install. But it may be the RECORD-less blocker, so
    # it counts as unresolved; "could not determine the tree is clean" must
    # not read as clean.
    dead = _make_dist_info(
        tmp_path,
        "esphome-device-builder",
        "1.0.9",
        with_metadata=False,
        with_record=False,
    )
    _fail_iterdir_for(monkeypatch, dead)
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 1)
    assert dead.is_dir()


def test_dedupe_scoped_ignores_an_unlistable_non_target(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The unresolved count only covers in-scope entries: the scoped
    # device-builder heal must not fail over an unlistable directory it was
    # never going to touch.
    broken = _make_dist_info(
        tmp_path, "zeroconf", "0.147.0", with_metadata=False, with_record=False
    )
    _fail_iterdir_for(monkeypatch, broken)
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
    assert broken.is_dir()


def test_dedupe_counts_a_stale_duplicate_it_cannot_remove(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # A surviving duplicate keeps the #190 pileup in place, so importlib still
    # cannot resolve a single version; reporting that run as a success would
    # let the lazy heal claim it fixed a tree it did not.
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    stale = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9")

    def refuse(_path: Path) -> None:
        raise OSError("locked")

    monkeypatch.setattr(maint, "_rmtree", refuse)
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 1)
    assert keep.is_dir()
    assert stale.is_dir()


def test_dedupe_infers_name_from_a_version_less_directory(tmp_path: Path) -> None:
    # A hand-damaged dir name with dashes and no version tail: the segment
    # after the last dash is not a version, so the whole stem is the name and
    # the scoped heal still condemns the RECORD-less entry instead of
    # mis-attributing it to "esphome-device" and skipping it.
    dead = tmp_path / "esphome-device-builder.dist-info"
    dead.mkdir()
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (1, 0)
    assert not dead.exists()


def test_dedupe_keeps_metadata_less_entry_with_record(tmp_path: Path) -> None:
    # Torn the other way: METADATA gone but RECORD intact. pip can still
    # uninstall this one, so the next upgrade heals it without our help;
    # deleting it here would discard the file list pip needs for that
    # uninstall.
    keep = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    torn = _make_dist_info(
        tmp_path, "esphome-device-builder", "1.0.9", with_metadata=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
    assert keep.is_dir()
    assert torn.is_dir()


def test_dedupe_inferred_name_never_ranks(tmp_path: Path) -> None:
    # A dir-name-only identity is enough to condemn a RECORD-less entry, but
    # never to rank one: a corrupted dist-info whose METADATA lost its Name but
    # kept a high-sorting Version, and whose directory name collides with a
    # real package, must not evict that package's genuine dist-info, and must
    # not be evicted itself on the strength of the collision.
    imposter = _make_dist_info(tmp_path, "esphome", "9999.0.0", with_name=False)
    real = _make_dist_info(tmp_path, "esphome", "2026.7.1")
    stale = _make_dist_info(tmp_path, "esphome", "2026.7.0")
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == (1, 0)
    assert imposter.is_dir()
    assert real.is_dir()
    assert not stale.exists()


def test_dedupe_default_scope_leaves_non_target_record_less(tmp_path: Path) -> None:
    # The lazy update-check heal stays scoped to the builder packages even for
    # RECORD-less entries; the post-copy dedupe-all owns the whole tree.
    dead = _make_dist_info(
        tmp_path, "zeroconf", "0.147.0", with_metadata=False, with_record=False
    )
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
    assert dead.is_dir()


def test_dedupe_all_removes_any_record_less(tmp_path: Path) -> None:
    # The #183 dev-channel shape was a torn zeroconf; dedupe-all must clear a
    # RECORD-less entry for any package, healthy sibling or not.
    dead = _make_dist_info(
        tmp_path, "zeroconf", "0.147.0", with_metadata=False, with_record=False
    )
    lone_dead = _make_dist_info(
        tmp_path, "bleak", "2.1.0", with_metadata=False, with_record=False
    )
    keep = _make_dist_info(tmp_path, "zeroconf", "0.148.0")
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == (2, 0)
    assert keep.is_dir()
    assert not dead.exists()
    assert not lone_dead.exists()


# --------------------------------------------------------------------------- #
# _clear_readonly_and_retry: the rmtree error handler. The flag-clearing leg
# only ever fires on Windows, so its contract is pinned directly here where it
# runs on every platform.
# --------------------------------------------------------------------------- #


def test_retry_handler_reraises_when_the_path_is_writable(tmp_path: Path) -> None:
    # A writable path that still failed means the read-only flag was not the
    # problem; the original error must surface unchanged and nothing may be
    # retried against the same wall.
    target = tmp_path / "file.txt"
    target.write_text("")
    original = OSError("boom")
    with pytest.raises(OSError) as caught:
        maint._clear_readonly_and_retry(os.unlink, str(target), original)
    assert caught.value is original
    assert target.exists()


_ROOT_SEES_EVERYTHING_WRITABLE = pytest.mark.skipif(
    hasattr(os, "geteuid") and os.geteuid() == 0,
    reason="os.access(W_OK) is always true for root, so the handler re-raises "
    "instead of retrying; the test's premise needs an unprivileged user",
)


@_ROOT_SEES_EVERYTHING_WRITABLE
def test_retry_handler_clears_the_flag_and_retries(tmp_path: Path) -> None:
    target = tmp_path / "file.txt"
    target.write_text("")
    target.chmod(0o444)
    maint._clear_readonly_and_retry(os.unlink, str(target), OSError("original"))
    assert not target.exists()


def test_rmtree_wires_the_onerror_adapter_below_312(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # The onerror fallback only ever runs on a dev system Python older than
    # 3.12, where CI never executes, so pin the adapter's exc_info indexing
    # here: a wrong index would otherwise surface only on the machine the
    # branch was written to protect, as a TypeError inside the rmtree walk.
    captured: dict[str, object] = {}

    def fake_rmtree(path: Path, onerror: object = None) -> None:
        captured["onerror"] = onerror

    monkeypatch.setattr(maint.shutil, "rmtree", fake_rmtree)
    # The import-time constant, not sys.version_info: patching the latter is
    # process-wide and can confuse anything else that reads it mid-test.
    monkeypatch.setattr(maint, "_RMTREE_HAS_ONEXC", False)
    maint._rmtree(tmp_path)
    onerror = captured["onerror"]
    assert callable(onerror)
    # The writable early-out must re-raise the exception *instance* out of
    # the (type, value, traceback) triple, which is what proves the [1].
    target = tmp_path / "file.txt"
    target.write_text("")
    original = OSError("boom")
    with pytest.raises(OSError) as caught:
        onerror(os.unlink, str(target), (OSError, original, None))
    assert caught.value is original
    assert target.exists()


@_ROOT_SEES_EVERYTHING_WRITABLE
def test_retry_handler_chains_a_failed_retry(tmp_path: Path) -> None:
    # The retry failing anew must not silently replace the original cause; the
    # chain keeps both in the traceback the "could not remove" line reports.
    target = tmp_path / "file.txt"
    target.write_text("")
    target.chmod(0o444)
    original = OSError("original")

    def refuse(_path: str) -> None:
        raise OSError("still locked")

    with pytest.raises(OSError) as caught:
        maint._clear_readonly_and_retry(refuse, str(target), original)
    assert caught.value.__cause__ is original


# --------------------------------------------------------------------------- #
# main(): the argv contract the Rust runners invoke, including the exit code
# that is the only channel by which "still corrupt" reaches the caller.
# --------------------------------------------------------------------------- #


def test_main_detect_prints_the_version(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    _make_dist_info(tmp_path, "esphome-device-builder", "1.0.10")
    monkeypatch.setattr(maint, "distributions", lambda: _dists(tmp_path))
    assert maint.main(["detect"]) == 0
    assert capsys.readouterr().out.strip() == "1.0.10"


def test_main_dedupe_scopes_to_targets(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # dedupe must leave a non-target RECORD-less entry to dedupe-all: a swap
    # of the scope selection would turn the scoped heal into a whole-tree
    # prune on a live user tree.
    dead = _make_dist_info(
        tmp_path, "zeroconf", "0.147.0", with_metadata=False, with_record=False
    )
    monkeypatch.setattr(maint, "distributions", lambda: _dists(tmp_path))
    assert maint.main(["dedupe"]) == 0
    assert capsys.readouterr().out.strip() == "0"
    assert dead.is_dir()
    assert maint.main(["dedupe-all"]) == 0
    assert capsys.readouterr().out.strip() == "1"
    assert not dead.exists()


def test_main_exits_non_zero_when_a_record_less_entry_survives(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    # The Rust caller only looks at the exit status; a surviving RECORD-less
    # entry reported through exit 0 would read as a heal.
    _make_dist_info(
        tmp_path,
        "esphome-device-builder-frontend",
        "0.1.150",
        with_metadata=False,
        with_record=False,
    )
    monkeypatch.setattr(maint, "distributions", lambda: _dists(tmp_path))

    def refuse(_path: Path) -> None:
        raise OSError("locked")

    monkeypatch.setattr(maint, "_rmtree", refuse)
    assert maint.main(["dedupe"]) == 1
    captured = capsys.readouterr()
    assert captured.out.strip() == "0"
    assert "could not remove RECORD-less" in captured.err


def test_main_rejects_an_unknown_mode(capsys: pytest.CaptureFixture[str]) -> None:
    assert maint.main(["bogus"]) == 2
    assert maint.main([]) == 2
    assert "unknown mode" in capsys.readouterr().err


def test_dedupe_all_counts_an_unattributable_dist_info(tmp_path: Path) -> None:
    # A directory name that yields no package name at all cannot be
    # attributed, ranked, or condemned; in all-scope mode that is unresolved
    # damage and must not read as a heal.
    weird = tmp_path / "-1.0.dist-info"
    weird.mkdir()
    assert maint.dedupe_dist_info(_dists(tmp_path), targets=None) == (0, 1)
    assert weird.is_dir()


def test_dedupe_scoped_skips_an_unattributable_dist_info(tmp_path: Path) -> None:
    # In target scope the nameless entry cannot be tied to the builder
    # packages, so it is left for dedupe-all without failing the scoped heal.
    weird = tmp_path / "-1.0.dist-info"
    weird.mkdir()
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 0)
    assert weird.is_dir()


def test_dedupe_all_counts_a_distribution_without_a_usable_path() -> None:
    # The private _path guard: if importlib ever changes shape, every entry
    # lands here and the prune can do nothing at all; a total no-op must not
    # read as a heal in all-scope mode. Target scope skips it uncounted,
    # since without a path there is no name to scope-filter on.
    pathless = SimpleNamespace(_path="not-a-path")
    assert maint.dedupe_dist_info([pathless], targets=None) == (0, 1)
    assert maint.dedupe_dist_info([pathless]) == (0, 0)


def test_dedupe_counts_an_unreadable_metadata_entry(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # METADATA exists but cannot be read: unlike the absent-METADATA torn
    # shape (a deliberate keep), a transient read failure hides whether this
    # entry drives the version-None pileup, so the run must not report a
    # heal around it. The entry itself is kept; deleting on a read failure
    # is the one wrong the prune must never commit.
    entry = _make_dist_info(tmp_path, "esphome-device-builder", "1.0.9")

    def unreadable_metadata(self: PathDistribution) -> object:
        raise OSError("transient read failure")

    monkeypatch.setattr(PathDistribution, "metadata", property(unreadable_metadata))
    assert maint.dedupe_dist_info(_dists(tmp_path)) == (0, 1)
    assert entry.is_dir()
