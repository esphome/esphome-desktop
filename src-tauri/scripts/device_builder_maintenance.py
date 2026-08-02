#!/usr/bin/env python3
"""Maintenance for the bundled esphome-device-builder install (#190).

Three modes, selected by argv:
  detect     -> print the highest installed version (empty if undeterminable)
  dedupe     -> remove orphaned duplicate *.dist-info dirs for the device-builder
                packages; print the count removed
  dedupe-all -> the same prune over every installed distribution; print the
                count removed. Run after each bundled-tree copy so a dirty
                source (the installer overlays the bundle without deleting the
                previous release's dist-info dirs, #389) still yields a copy
                with unambiguous importlib.metadata answers.

The bundled Python accumulates duplicate dist-info dirs (orphaned by the
``--ignore-installed`` missing-RECORD recovery), which makes importlib.metadata
return None or the wrong version and loops the updater forever. The version
ranking is self-contained so this does not depend on the third-party
``packaging`` library being importable in the bundled interpreter.

Both dedupe modes also remove RECORD-less dist-info dirs. pip cannot uninstall
an entry with no RECORD file, so every upgrade that touches the package aborts
with ``uninstall-no-record-file`` ("Cannot uninstall <pkg> None", or with the
version when METADATA survived), and because the installer overlay can plant
one in the bundled tree itself (#389), the repair re-copy restores it and the
failure becomes permanent. A completed pip install always writes RECORD, so
this shape is torn metadata rather than a real install; deleting the dist-info
dir loses nothing pip could use, and the next install writes a fresh one.
In-scope damage the prune could not resolve — a removal that failed, a
directory it could not list — fails the run (exit 1): a partial prune must
never read as a heal.

Embedded into the Rust binary via ``include_str!`` and run with the bundled
interpreter as ``python -c <this file> <mode>``; also imported directly by the
pytest suite, which is why the functions take an injectable distributions
iterable instead of always reading the live environment.
"""

from __future__ import annotations

import os
import re
import shutil
import stat
import sys
from collections.abc import Callable, Iterable
from importlib.metadata import Distribution, distributions
from pathlib import Path

# Only the builder packages: the device-builder updater resolves its version via
# importlib.metadata, which the duplicate dist-info pileup breaks (#190). Plain
# esphome is read with `python -m esphome version` (runtime import resolution),
# so its own --ignore-installed orphans never trigger this loop and are left
# alone here.
TARGETS = {"esphome-device-builder", "esphome-device-builder-frontend"}

# Pre-release precedence (PEP 440 order): dev < a < b < rc < release. A release
# segment with no pre-release tag sorts above any pre-release of the same
# version, so it gets the high sentinel 9.
_ORDER = {
    None: 9,
    "dev": 0,
    "a": 1,
    "alpha": 1,
    "b": 2,
    "beta": 2,
    "c": 3,
    "rc": 3,
    "pre": 3,
    "preview": 3,
}
# Order the tag alternation longest-first so a spelled-out tag is matched whole
# (e.g. "alpha2" -> tag "alpha", serial 2) instead of the leading "a" winning and
# dropping the serial.
_VER_RE = re.compile(
    r"^\s*v?(\d+(?:\.\d+)*)"
    r"(?:[-_.]?(alpha|beta|preview|rc|pre|dev|a|b|c)\.?(\d*))?"
)


# Sort key returned for any version we cannot parse; sorts below every real
# version so an unparseable entry never wins a "highest version" comparison.
_UNRANKED: tuple[tuple[int, ...], int, int] = ((), 0, 0)


def vkey(version: str | None) -> tuple[tuple[int, ...], int, int]:
    """Return a PEP 440-ish sort key; unparseable/None sorts lowest."""
    # None, "", "None" and any other non-version string all fail to match
    # (no leading digits) and fall through to the lowest key.
    match = _VER_RE.match(str(version or "").lower())
    if not match:
        return _UNRANKED
    release = tuple(int(x) for x in match.group(1).split("."))
    return (release, _ORDER.get(match.group(2), 4), int(match.group(3) or 0))


def _norm(name: str | None) -> str:
    return (name or "").lower().replace("_", "-")


def _dist_path(dist: Distribution) -> object:
    """``dist``'s private ``_path`` for log messages, or ``"?"`` if absent."""
    return getattr(dist, "_path", "?")


def _clear_readonly_and_retry(
    func: Callable[[str], object], failed: str, exc: BaseException
) -> None:
    """``shutil.rmtree`` error handler: clear a read-only flag, retry once.

    A path that is writable yet still failed re-raises the original error:
    the flag is not the problem there, and retrying would just fail again. A
    retry that fails anew chains the original error so the log names both
    causes. Module-level rather than nested in [`_rmtree`] so the early-out
    and the chain are unit-testable on every platform; the flag-clearing leg
    only ever fires on Windows.
    """
    if os.access(failed, os.W_OK):
        raise exc
    try:
        Path(failed).chmod(stat.S_IWUSR | stat.S_IRUSR | stat.S_IXUSR)
        func(failed)
    except OSError as retry_err:
        raise retry_err from exc


def _rmtree(path: Path) -> None:
    """``shutil.rmtree`` that clears Windows' read-only flag and retries.

    Files inside a dist-info can carry the read-only attribute on Windows,
    which fails the plain delete (same handling as ``esphome.helpers.rmtree``,
    plus the execute bit so a chmod'd directory stays traversable for the
    retry).
    """
    if sys.version_info >= (3, 12):
        shutil.rmtree(path, onexc=_clear_readonly_and_retry)
        return

    # The bundled interpreter is far newer, but dev builds without a bundle
    # fall back to the system Python, which can predate ``onexc`` (3.12); the
    # resulting TypeError would escape the callers' ``except OSError`` and
    # kill the whole prune. ``onerror`` passes an exc_info triple instead of
    # the exception, so adapt.
    def _onerror(
        func: Callable[[str], object],
        failed: str,
        exc_info: tuple[type[BaseException], BaseException, object],
    ) -> None:
        _clear_readonly_and_retry(func, failed, exc_info[1])

    shutil.rmtree(path, onerror=_onerror)


def _infer_name(path: Path) -> str:
    """Normalized package name from a ``*.dist-info`` directory name.

    ``pkg_name-1.2.3.dist-info`` -> ``pkg-name``; when the tail after the last
    ``-`` does not look like a version, the whole stem is taken as the name.
    The wheel spec underscore-normalizes the name part, so the last ``-``
    always cuts at the version for anything pip wrote, and pip resolves
    identity from the directory name the same way when METADATA is gone
    ("Cannot uninstall esphome-device-builder-frontend None"). The digit check
    only matters for hand-damaged names like ``foo-bar.dist-info``, which must
    not shed their last segment on the way into the scope filter.

    Only used when METADATA is missing or has no Name header, and only to
    scope-filter the entry and attribute the RECORD-less removal; an inferred
    name never ranks its entry against properly named siblings.
    """
    stem = path.name[: -len(".dist-info")]
    name, sep, version = stem.rpartition("-")
    if sep and version[:1].isdigit():
        return _norm(name)
    return _norm(stem)


def detect_version(dists: Iterable[Distribution]) -> str | None:
    """Return the highest version among all esphome-device-builder dists.

    Enumerating every matching distribution and taking the max is robust to the
    duplicate dist-info pileup that makes ``version('esphome-device-builder')``
    return None or an arbitrary older version.
    """
    versions: list[str] = []
    for dist in dists:
        try:
            # Use .get() rather than mapping access: a missing header returns
            # None instead of emitting the implicit-None DeprecationWarning that
            # becomes a KeyError in future Python.
            meta = dist.metadata
            if _norm(meta.get("Name")) == "esphome-device-builder":
                version = meta.get("Version")
                if version and version != "None":
                    versions.append(version)
        except Exception as err:
            # Don't let one unreadable distribution abort detection, but log it:
            # silently dropping the real target would reintroduce the #190 loop
            # with no trace.
            print(
                f"detect: skipping unreadable distribution {_dist_path(dist)}: {err}",
                file=sys.stderr,
            )
    return max(versions, key=vkey) if versions else None


def dedupe_dist_info(
    dists: Iterable[Distribution], targets: set[str] | None = TARGETS
) -> tuple[int, int]:
    """Keep the highest-version dist-info per package; remove the rest.

    ``targets`` limits the prune to the given normalized package names;
    ``None`` considers every distribution (the ``dedupe-all`` mode).

    The newest version is the code installed last by ``pip install --upgrade``,
    so its metadata is the one to keep. RECORD-less entries are removed
    regardless of group size or version readability: pip can never uninstall
    one, so it aborts every upgrade with ``uninstall-no-record-file`` until it
    is gone. Returns ``(removed, failed)``: dist-info directories removed, and
    in-scope damage the prune could not resolve — a RECORD-less entry it could
    not remove (still aborts every install), a stale duplicate it could not
    remove (still breaks version detection, #190), or a directory it could not
    even list (may be any of those). The CLI turns ``failed`` into a non-zero
    exit so a partial prune is never reported as a heal.
    """
    removed = 0
    failed = 0
    groups: dict[str, list[tuple[str | None, Path, bool, bool]]] = {}
    for dist in dists:
        # ``_path`` is private; guard it so a future importlib change degrades to
        # a no-op rather than deleting the wrong directory.
        path = getattr(dist, "_path", None)
        if (
            not isinstance(path, Path)
            or path.suffix != ".dist-info"
            or not path.is_dir()
        ):
            continue
        name = ""
        version = None
        try:
            # .get() avoids the implicit-None DeprecationWarning (future
            # KeyError) on missing headers, and reading Version here keeps a
            # broken target's metadata from crashing the whole prune.
            meta = dist.metadata
            name = _norm(meta.get("Name"))
            version = meta.get("Version")
        except Exception as err:
            # Log rather than silently skip: an unreadable target dist-info that
            # is never considered for dedup leaves the pileup in place (#190).
            print(
                f"dedupe: unreadable metadata in {path}: {err}",
                file=sys.stderr,
            )
        inferred = not name
        if inferred:
            # No METADATA, or one with no Name header. The directory name still
            # identifies the package well enough to scope-filter the entry;
            # without it the torn dist-info behind the permanent
            # uninstall-no-record-file abort could never be considered at all.
            name = _infer_name(path)
        if not name:
            print(
                f"dedupe: skipping nameless distribution {path}",
                file=sys.stderr,
            )
            continue
        if targets is not None and name not in targets:
            continue
        try:
            children = {child.name for child in path.iterdir()}
        except OSError as err:
            # A directory that cannot even be listed decides nothing, least of
            # all its own removal: a transient read failure must not turn
            # "RECORD not seen" into "RECORD absent" and delete a real install.
            # But it is counted: this entry may be the RECORD-less blocker,
            # and "could not determine the tree is clean" must not read as
            # clean. After the scope filter so an unlistable non-target dir
            # cannot fail the scoped heal.
            failed += 1
            print(f"dedupe: could not list {path}: {err}", file=sys.stderr)
            continue
        groups.setdefault(name, []).append(
            (version, path, "RECORD" in children, inferred)
        )

    for entries in groups.values():
        items: list[tuple[str | None, Path]] = []
        for version, path, has_record, inferred in entries:
            if not has_record:
                # No RECORD: pip can never uninstall this entry, so any
                # install that tries aborts with uninstall-no-record-file,
                # whether or not the version is still readable ("Cannot
                # uninstall <pkg> None", or with the version when METADATA
                # survived). A completed pip install always writes RECORD, so
                # unlike the unrankable-but-managed case below this cannot be
                # the real install, and removing the dist-info dir is the only
                # way out of the abort loop.
                print(f"dedupe: removing RECORD-less {path}", file=sys.stderr)
                try:
                    _rmtree(path)
                    removed += 1
                except OSError as err:
                    # Counted, not just logged: this entry still blocks every
                    # install, so the run must not report success around it.
                    # Worded apart from the best-effort "skip" below so a
                    # bounded stderr tail says which failure drove the exit 1.
                    failed += 1
                    print(
                        f"dedupe: could not remove RECORD-less {path}: {err}",
                        file=sys.stderr,
                    )
                continue
            if inferred:
                # A directory name is identity enough to condemn a
                # metadata-dead entry above (pip trusts it the same way when
                # it aborts), but not to rank the entry against properly named
                # siblings: a corrupted name landing in another package's
                # group could evict that package's real dist-info. Keep it
                # and keep it out of the ranking.
                print(f"dedupe: keeping dir-name-only {path}", file=sys.stderr)
                continue
            items.append((version, path))
        if len(items) < 2:
            continue  # a single healthy install is left untouched
        items.sort(key=lambda item: vkey(item[0]))
        keep_version, keep_path = items[-1]
        if vkey(keep_version) == _UNRANKED:
            # No entry in the group has a parseable version, so we can't tell
            # which is the real install. Leave the whole group rather than risk
            # a wrong rmtree; detect_version still tolerates the duplicates.
            print(
                f"dedupe: keeping ambiguous group, no parseable version "
                f"near {keep_path}",
                file=sys.stderr,
            )
            continue
        for version, path in items[:-1]:
            if vkey(version) == _UNRANKED:
                # An unparseable version with a RECORD (RECORD-less entries
                # never reach this loop) might itself be the real install, so
                # never delete it on the strength of the lowest-sort sentinel.
                print(f"dedupe: keeping unrankable {path}", file=sys.stderr)
                continue
            try:
                _rmtree(path)
                removed += 1
            except OSError as err:
                # Also counted: a surviving duplicate keeps the #190 pileup in
                # place, so importlib still cannot resolve a single version
                # and the updater loops on "version None". Worded apart from
                # the RECORD-less and unlistable lines so a bounded stderr
                # tail says which failure drove the exit 1.
                failed += 1
                print(
                    f"dedupe: could not remove stale duplicate {path}: {err}",
                    file=sys.stderr,
                )
    return removed, failed


def main(argv: list[str]) -> int:
    mode = argv[0] if argv else ""
    if mode == "detect":
        version = detect_version(distributions())
        if version:
            print(version)
        return 0
    if mode in ("dedupe", "dedupe-all"):
        removed, failed = dedupe_dist_info(
            distributions(), targets=TARGETS if mode == "dedupe" else None
        )
        print(removed)
        # Damage the prune could not resolve still blocks the updater (a
        # RECORD-less dir aborts every install, a surviving duplicate breaks
        # version detection); exit 1 so a partial prune is never reported as
        # a heal. The callers are best-effort and continue either way, so the
        # exit code buys a distinguishable log line, not a control-flow guard.
        return 1 if failed else 0
    print(f"unknown mode: {mode!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
