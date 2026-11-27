#!/usr/bin/env python3
"""Resolve the exact tauri-cli version a Build run installs everywhere.

Reads the `tauri` crate version locked in src-tauri/Cargo.lock, then picks the
newest non-yanked, non-prerelease tauri-cli release on crates.io in the same
major.minor line. The `tauri` library and `tauri-cli` publish on independent
cadences (library 2.11.5 can have no matching CLI 2.11.5), so the minor line,
not the exact patch, is what has to match; Tauri keeps CLI patches within a
minor compatible with the library.

Resolving once per run, in its own job, matters for release builds: build.yml
compiles the app with one tauri-cli and bundle-sign-upload.yml's bundle jobs
patch and bundle it with another, and a CLI patch published between the two
would otherwise split them across versions.

Emits `version=<x.y.z>` as a step output (stdout when run locally).

Usage:
  python resolve_tauri_cli_version.py [--lock PATH] [--index-url URL]
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
import urllib.request
from pathlib import Path

_SCRIPT_DIR = str(Path(__file__).resolve().parent)
if _SCRIPT_DIR not in sys.path:
    sys.path.insert(0, _SCRIPT_DIR)

from _gha import emit_outputs, error  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_LOCK = REPO_ROOT / "src-tauri" / "Cargo.lock"
# crates.io sparse index entry for tauri-cli: one JSON object per published
# version, with `vers` and `yanked`.
DEFAULT_INDEX_URL = "https://index.crates.io/ta/ur/tauri-cli"
USER_AGENT = "esphome-desktop-ci (resolve_tauri_cli_version.py)"

# Plain release versions only: no prerelease or build metadata, and nothing a
# later $GITHUB_OUTPUT or shell use could misread.
RELEASE_VERSION_RE = re.compile(r"^(\d+)\.(\d+)\.(\d+)$")


class ResolveError(Exception):
    """The version could not be resolved from the inputs given."""


def locked_tauri_version(lock_text: str) -> str:
    """Return the version of the `tauri` package locked in a Cargo.lock."""
    lock = tomllib.loads(lock_text)
    versions = [
        pkg.get("version", "")
        for pkg in lock.get("package", [])
        if pkg.get("name") == "tauri"
    ]
    if len(versions) != 1:
        raise ResolveError(
            f"expected exactly one locked `tauri` package, found {len(versions)}"
        )
    if not RELEASE_VERSION_RE.match(versions[0]):
        raise ResolveError(f"unexpected locked tauri version: {versions[0]!r}")
    return versions[0]


def newest_cli_in_minor(index_text: str, library_version: str) -> str:
    """Pick the newest usable tauri-cli in the library version's minor line."""
    match = RELEASE_VERSION_RE.match(library_version)
    if match is None:
        raise ResolveError(f"unexpected tauri version: {library_version!r}")
    major, minor = int(match.group(1)), int(match.group(2))

    candidates: list[tuple[int, int, int]] = []
    for line in index_text.splitlines():
        if not line.strip():
            continue
        entry = json.loads(line)
        if entry.get("yanked"):
            continue
        parsed = RELEASE_VERSION_RE.match(str(entry.get("vers", "")))
        if parsed is None:
            continue
        version = (int(parsed.group(1)), int(parsed.group(2)), int(parsed.group(3)))
        if version[:2] == (major, minor):
            candidates.append(version)
    if not candidates:
        raise ResolveError(f"no published tauri-cli release matches ~{major}.{minor}")
    return ".".join(str(part) for part in max(candidates))


def fetch_index(url: str) -> str:
    """Fetch a crates.io sparse index file."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=30) as response:
        return response.read().decode("utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--lock", type=Path, default=DEFAULT_LOCK)
    parser.add_argument("--index-url", default=DEFAULT_INDEX_URL)
    args = parser.parse_args(argv)

    try:
        library = locked_tauri_version(args.lock.read_text(encoding="utf-8"))
        cli = newest_cli_in_minor(fetch_index(args.index_url), library)
    except (OSError, ValueError, ResolveError) as exc:
        error(f"could not resolve the tauri-cli version: {exc}")
        return 1
    print(f"Resolved tauri-cli {cli} (locked tauri {library})", file=sys.stderr)
    emit_outputs(version=cli)
    return 0


if __name__ == "__main__":
    sys.exit(main())
