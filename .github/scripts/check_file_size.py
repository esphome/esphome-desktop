#!/usr/bin/env python3
"""Enforce a line cap on the Rust sources under src-tauri/src.

Run from the repo root, with no arguments:

    python3 .github/scripts/check_file_size.py

Wired into the `lint-test` job (.github/workflows/lint-test.yml) and mirrored
by a pre-commit hook. Exits non-zero, with a message naming each offending
file, on any of:

  * a non-exempt file over the cap;
  * an exempt file that has dropped to the cap or below (remove it from
    EXEMPT so it can never regress);
  * an EXEMPT entry naming a file that no longer exists.

The cap counts *code* lines: the trailing `#[cfg(test)] mod` block does not
count against it. Rust inlines its unit tests, so counting them would mean a
700-line file with 200 lines of tests is "over" and the cheapest way back
under is to delete tests. See CONTRIBUTING.md ("Code structure policies") for
the rule as contributors read it.
"""

from __future__ import annotations

import subprocess
import sys
from collections.abc import Iterable
from pathlib import Path

CAP = 800

# Files that were already over the cap when it was introduced. They are
# grandfathered, not pinned: an exempt file may grow, because #331 rewrites
# 2080 lines of platform/mod.rs and must not be blocked by a cap it predates.
#
# The list only shrinks. Once a file drops to the cap or below, the check
# fails until its entry is deleted, and from then on the cap holds it there.
EXEMPT: frozenset[str] = frozenset(
    {
        # ~84 top-level items and no siblings; #342 splits this one.
        "src-tauri/src/platform/mod.rs",
        "src-tauri/src/update/mod.rs",
        "src-tauri/src/tray/mod.rs",
        "src-tauri/src/control/client.rs",
        "src-tauri/src/control/ops.rs",
        "src-tauri/src/lib.rs",
    }
)


def code_line_count(source: str) -> int:
    """Count the lines of `source` that are not inside a test module.

    A test module is a column-0 `#[cfg(test)]` attribute whose next non-blank
    line declares a `mod`; it ends at the next column-0 `}`. Every such block
    is skipped and everything else counts, *including* code that follows one.

    Each part of that is load-bearing, and the simpler versions are all wrong
    on this tree:

      * `mod`, not just the attribute. `i18n/mod.rs` and `util/mod.rs` gate a
        *function* on `#[cfg(test)]` partway up the file; keying on the
        attribute alone scores i18n/mod.rs as 52 lines.
      * Column 0. `platform/mod.rs` nests `#[cfg(test)] mod tests` blocks
        inside its per-OS `mod macos` / `mod windows` blocks; keying on any
        `mod tests` scores it as 1722.
      * Skip the block, do not stop at it. Truncating at the first test module
        means one placed mid-file silently exempts the whole rest of the file:
        a 5000-line file scores 1 and passes. That is not hypothetical
        gaming — grouping a test module beside the code it covers, rather than
        at the bottom, is a normal thing to do.

    Finding the end of a block by the next column-0 `}` leans on rustfmt
    putting the closing brace of a top-level item at column 0, which holds
    because `cargo fmt --check` is a blocking gate here (CONTRIBUTING.md). It
    is a line-based heuristic, not a Rust parser, which is proportionate for a
    navigability guardrail.

    Nested (indented) test modules are not matched and so count as code, which
    overcounts platform/mod.rs by the ~560 lines of its two inner test
    modules. That is the safe direction: it can only ever report a file as
    larger than it is, never smaller.
    """
    lines = source.splitlines()
    total = 0
    index = 0
    while index < len(lines):
        # rstrip only, so a leading space disqualifies a nested attribute.
        if lines[index].rstrip() == "#[cfg(test)]":
            declaration = index + 1
            while declaration < len(lines) and not lines[declaration].strip():
                declaration += 1
            if declaration < len(lines) and lines[declaration].startswith("mod "):
                if lines[declaration].rstrip().endswith(";"):
                    # `#[cfg(test)] mod tests;` — the body is another file.
                    index = declaration + 1
                    continue
                end = declaration + 1
                while end < len(lines) and lines[end].rstrip() != "}":
                    end += 1
                index = end + 1
                continue
        total += 1
        index += 1
    return total


def tracked_rust_files(root: Path) -> list[str]:
    """List the tracked .rs files under src-tauri/src, via git.

    `git ls-files` rather than a filesystem walk, deliberately: the bundled
    Python tree carries 20k-line vendored sources and a working tree may hold
    an untracked nested checkout of this repo. Neither is ours to measure and
    neither is tracked.
    """
    out = subprocess.run(
        ["git", "ls-files", "-z", "src-tauri/src/*.rs", "src-tauri/src/**/*.rs"],
        cwd=root,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return sorted(entry for entry in out.split("\0") if entry)


def check(root: Path, files: Iterable[str], exempt: Iterable[str]) -> list[str]:
    """Return a message for every violation; empty means the tree is clean."""
    exempt = set(exempt)
    failures: list[str] = []
    seen: set[str] = set()

    for relative in files:
        count = code_line_count((root / relative).read_text(encoding="utf-8"))
        if relative in exempt:
            seen.add(relative)
            if count <= CAP:
                failures.append(
                    f"{relative} is down to {count} code lines, at or under the "
                    f"{CAP} cap. Remove it from EXEMPT in "
                    f".github/scripts/check_file_size.py so it stays there."
                )
        elif count > CAP:
            failures.append(
                f"{relative} has {count} code lines, over the {CAP} cap. Split "
                f"it into submodules; see CONTRIBUTING.md (Code structure "
                f"policies). The cap does not count the trailing "
                f"#[cfg(test)] mod block."
            )

    for stale in sorted(exempt - seen):
        failures.append(
            f"{stale} is in EXEMPT but is not a tracked file under "
            f"src-tauri/src. Remove the stale entry from "
            f".github/scripts/check_file_size.py."
        )

    return failures


def main() -> int:
    root = Path(__file__).resolve().parents[2]
    failures = check(root, tracked_rust_files(root), EXEMPT)
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
