#!/usr/bin/env python3
"""Check a downloaded artifact directory holds exactly the expected release files.

bundle-sign-upload.yml downloads the run's artifacts and uploads every file it
got to the draft release, so whatever lands in that directory ships. The build
matrix runs third-party code, and any job in the run can upload an artifact, so
the directory is checked against a fixed list first: every expected file
present exactly once, and nothing else. An extra file (a stray intermediate, a
translation JSON, an unsigned binary with a plausible name) fails the release
rather than being published next to the signed ones. Each artifact must also
sit in its own directory and hold only the file it is named after (see
check_layout), so one artifact cannot carry a replacement for another.

Two sets are checked:

- `release`: everything upload-release-assets ships (the installers, the
  updater bundles and every updater .sig).
- `linux-bundles`: the six unsigned Linux bundles sign-linux signs.

Usage:
  python check_release_artifacts.py {release,linux-bundles} DIR
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

_SCRIPT_DIR = str(Path(__file__).resolve().parent)
if _SCRIPT_DIR not in sys.path:
    sys.path.insert(0, _SCRIPT_DIR)

from _gha import error  # noqa: E402

# (label, pattern for the file name). Names follow Tauri's bundle naming:
# product name, version, then the arch spelling each format uses.
LINUX_BUNDLES: list[tuple[str, re.Pattern[str]]] = [
    ("linux x64 AppImage", re.compile(r".+_amd64\.AppImage$")),
    ("linux arm64 AppImage", re.compile(r".+_aarch64\.AppImage$")),
    ("linux x64 deb", re.compile(r".+_amd64\.deb$")),
    ("linux arm64 deb", re.compile(r".+_arm64\.deb$")),
    ("linux x64 rpm", re.compile(r".+\.x86_64\.rpm$")),
    ("linux arm64 rpm", re.compile(r".+\.aarch64\.rpm$")),
]

# Every updater payload ships with its .sig; the DMGs are download-only.
UPDATER_PAYLOADS: list[tuple[str, re.Pattern[str]]] = [
    ("windows NSIS installer", re.compile(r".+-setup\.exe$")),
    *LINUX_BUNDLES,
    ("macos arm64 updater bundle", re.compile(r".+_aarch64\.app\.tar\.gz$")),
    ("macos x64 updater bundle", re.compile(r".+_x64\.app\.tar\.gz$")),
]


def _with_signature(
    label: str, pattern: re.Pattern[str]
) -> tuple[str, re.Pattern[str]]:
    return f"{label} signature", re.compile(pattern.pattern[:-1] + r"\.sig$")


RELEASE: list[tuple[str, re.Pattern[str]]] = [
    *UPDATER_PAYLOADS,
    *(_with_signature(label, pattern) for label, pattern in UPDATER_PAYLOADS),
    ("macos arm64 DMG", re.compile(r".+_aarch64\.dmg$")),
    ("macos x64 DMG", re.compile(r".+_x64\.dmg$")),
]

EXPECTED_SETS: dict[str, list[tuple[str, re.Pattern[str]]]] = {
    "release": RELEASE,
    "linux-bundles": LINUX_BUNDLES,
}

# Artifacts uploaded as a zip of several files, and what those files must be.
# Every other artifact is a single file uploaded with `archive: false`, so it
# is named after that file.
MULTI_FILE_ARTIFACTS: dict[str, re.Pattern[str]] = {
    "linux-updater-signatures": re.compile(r".+\.sig$"),
}


def check_layout(root: Path) -> list[str]:
    """Return a problem per artifact whose contents don't match its name.

    download-artifact (without merge-multiple) puts each artifact in its own
    directory named after it. Downloading them all into one directory instead
    lets an artifact a build leg uploaded under any matching name (say
    `x.sig`) carry a file named like a signed installer and overwrite it, and
    the name check alone would still pass. Here each single-file artifact must
    hold exactly that one file, and a zipped one only files of its kind.
    """
    problems: list[str] = []
    for artifact in sorted(root.iterdir()):
        if artifact.is_symlink() or not artifact.is_dir():
            problems.append(f"not an artifact directory: {artifact.name}")
            continue
        contents = sorted(p for p in artifact.rglob("*"))
        if any(p.parent != artifact or p.is_dir() for p in contents):
            problems.append(f"{artifact.name} holds nested paths")
            continue
        names = [p.name for p in contents]
        kind = MULTI_FILE_ARTIFACTS.get(artifact.name)
        if kind is not None:
            if not names or not all(kind.match(name) for name in names):
                problems.append(f"{artifact.name} holds unexpected files: {names}")
        elif names != [artifact.name]:
            problems.append(f"artifact {artifact.name} holds {names}")
    return problems


def check(names: list[str], expected: list[tuple[str, re.Pattern[str]]]) -> list[str]:
    """Return a problem per missing, duplicated or unexpected file."""
    problems: list[str] = []
    claimed: set[str] = set()
    for label, pattern in expected:
        matches = sorted(name for name in names if pattern.match(name))
        if not matches:
            problems.append(f"missing {label} ({pattern.pattern})")
        elif len(matches) > 1:
            problems.append(f"more than one {label}: {', '.join(matches)}")
        claimed.update(matches)
    for name in sorted(set(names) - claimed):
        problems.append(f"unexpected file: {name}")
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("set", choices=sorted(EXPECTED_SETS))
    parser.add_argument("directory", type=Path)
    args = parser.parse_args(argv)

    if not args.directory.is_dir():
        error(f"{args.directory} is not a directory")
        return 1
    entries = sorted(args.directory.rglob("*"))
    names = [p.name for p in entries if p.is_file()]
    problems = check_layout(args.directory)
    problems += check(names, EXPECTED_SETS[args.set])
    # Anything that is neither a regular file nor a directory (a symlink
    # included, which is_file() follows) is rejected outright.
    for p in entries:
        if p.is_symlink() or not (p.is_file() or p.is_dir()):
            problems.append(f"not a regular file: {p.relative_to(args.directory)}")
    if problems:
        for problem in problems:
            error(problem)
        return 1
    print(f"{args.directory}: all {len(names)} expected {args.set} files present")
    return 0


if __name__ == "__main__":
    sys.exit(main())
