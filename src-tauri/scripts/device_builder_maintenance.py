#!/usr/bin/env python3
"""Maintenance for the bundled esphome-device-builder install (#190).

Two modes, selected by argv:
  detect  -> print the highest installed version (empty if undeterminable)
  dedupe  -> remove orphaned duplicate *.dist-info dirs; print the count removed

The bundled Python accumulates duplicate dist-info dirs (orphaned by the
``--ignore-installed`` missing-RECORD recovery), which makes importlib.metadata
return None or the wrong version and loops the updater forever. The version
ranking is self-contained so this does not depend on the third-party
``packaging`` library being importable in the bundled interpreter.

Embedded into the Rust binary via ``include_str!`` and run with the bundled
interpreter as ``python -c <this file> <mode>``; also imported directly by the
pytest suite, which is why the functions take an injectable distributions
iterable instead of always reading the live environment.
"""

from __future__ import annotations

import re
import shutil
import sys
from collections.abc import Iterable
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
            path = getattr(dist, "_path", "?")
            print(
                f"detect: skipping unreadable distribution {path}: {err}",
                file=sys.stderr,
            )
    return max(versions, key=vkey) if versions else None


def dedupe_dist_info(dists: Iterable[Distribution]) -> int:
    """Keep the highest-version dist-info per target package; remove the rest.

    The newest version is the code installed last by ``pip install --upgrade``,
    so its metadata is the one to keep. Returns the number of stale dist-info
    directories removed.
    """
    groups: dict[str, list[tuple[str | None, Path]]] = {}
    for dist in dists:
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
            path = getattr(dist, "_path", "?")
            print(
                f"dedupe: skipping unreadable distribution {path}: {err}",
                file=sys.stderr,
            )
            continue
        if name not in TARGETS:
            continue
        # ``_path`` is private; guard it so a future importlib change degrades to
        # a no-op rather than deleting the wrong directory.
        path = getattr(dist, "_path", None)
        if (
            not isinstance(path, Path)
            or path.suffix != ".dist-info"
            or not path.is_dir()
        ):
            continue
        groups.setdefault(name, []).append((version, path))

    removed = 0
    for items in groups.values():
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
                # An unparseable version might itself be the real install, so
                # never delete it on the strength of the lowest-sort sentinel.
                print(f"dedupe: keeping unrankable {path}", file=sys.stderr)
                continue
            try:
                shutil.rmtree(path)
                removed += 1
            except OSError as err:
                print(f"skip {path}: {err}", file=sys.stderr)
    return removed


def main(argv: list[str]) -> int:
    mode = argv[0] if argv else ""
    if mode == "detect":
        version = detect_version(distributions())
        if version:
            print(version)
        return 0
    if mode == "dedupe":
        print(dedupe_dist_info(distributions()))
        return 0
    print(f"unknown mode: {mode!r}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
