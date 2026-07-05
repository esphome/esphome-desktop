#!/usr/bin/env python3
"""Set the release version in every version-bearing file.

Run by .github/workflows/release-drafter.yml after the release draft is
(re)generated. Each entry in VERSION_FILES maps a file to the pattern for its
version line and the replacement line, so adding a new version-bearing file is
one line in the table instead of another bespoke sed step.

The script fails loudly (non-zero exit) if a pattern doesn't match its file —
that means the file was renamed or restructured and this table needs updating;
a silent no-op would ship a mismatched version and break download URLs built
from it.

Usage:

    python3 .github/scripts/set_version.py 1.2.3
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent.parent

# (file, version-line pattern, replacement template). Patterns are multiline so
# they anchor per line, and each replacement rewrites just the matched
# substring — the same edit the old per-file sed steps made.
VERSION_FILES: tuple[tuple[str, re.Pattern[str], str], ...] = (
    (
        "src-tauri/Cargo.toml",
        re.compile(r"^version = [^\r\n]*", re.MULTILINE),
        'version = "{version}"',
    ),
    (
        "src-tauri/tauri.conf.json",
        re.compile(r'"version": [^\r\n]*', re.MULTILINE),
        '"version": "{version}",',
    ),
    (
        "packaging/aur/esphome-desktop-bin/PKGBUILD",
        re.compile(r"^pkgver=[^\r\n]*", re.MULTILINE),
        "pkgver={version}",
    ),
)


def set_version(
    text: str, pattern: re.Pattern[str], template: str, version: str
) -> str:
    """Replace exactly one match of `pattern` with the filled-in `template`.

    Raises ValueError when the match count is not exactly one, so a renamed
    or restructured file fails the release job instead of silently shipping
    a stale version, and an over-broad pattern fails instead of silently
    rewriting extra lines.
    """
    replacement = template.format(version=version)
    # Function replacement so any backslashes/specials stay literal.
    new, count = pattern.subn(lambda _m: replacement, text)
    if count != 1:
        raise ValueError(
            f"pattern {pattern.pattern!r} matched {count} times, expected exactly 1"
        )
    return new


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", help="Release version, without the leading v.")
    parser.add_argument(
        "--root",
        default=str(REPO_ROOT),
        help="Repository root containing the version-bearing files.",
    )
    args = parser.parse_args(argv)

    root = Path(args.root)
    # Two phases: compute every rewrite first, write only after all patterns
    # matched, so a failure can't leave the tree partially bumped.
    rewritten: list[tuple[Path, str, str]] = []
    for rel_path, pattern, template in VERSION_FILES:
        path = root / rel_path
        try:
            text = path.read_text(encoding="utf-8")
            new = set_version(text, pattern, template, args.version)
        except (OSError, ValueError) as exc:
            print(f"::error::{rel_path}: {exc}", file=sys.stderr)
            return 1
        rewritten.append((path, rel_path, new))
    for path, rel_path, new in rewritten:
        try:
            path.write_text(new, encoding="utf-8")
        except OSError as exc:
            print(f"::error::{rel_path}: {exc}", file=sys.stderr)
            return 1
        print(f"{rel_path}: set version to {args.version}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
