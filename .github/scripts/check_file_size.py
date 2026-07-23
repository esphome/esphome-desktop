#!/usr/bin/env python3
"""Enforce a line cap on the Rust sources under src-tauri/src.

Run from the repo root, with no arguments:

    python3 .github/scripts/check_file_size.py   # `python` on Windows

(A python.org install on Windows puts `python.exe` on PATH but no
`python3.exe`; the pre-commit hook sidesteps this by having pre-commit
provision the interpreter.)

Wired into the `lint-test` job (.github/workflows/lint-test.yml) and mirrored
by a pre-commit hook. Exits non-zero, with a message naming each offending
file, on any of:

  * a non-exempt file over the cap;
  * an exempt file that has dropped to the cap or below (remove it from
    EXEMPT so it can never regress);
  * an EXEMPT entry naming a file that no longer exists.

The cap counts *code* lines: a top-level `#[cfg(test)] mod` block does not
count against it, wherever in the file it sits. Rust inlines its unit tests,
so counting them would mean a 700-line file with 200 lines of tests is "over"
and the cheapest way back under is to delete tests. See CONTRIBUTING.md
("Code structure policies") for the rule as contributors read it.
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
        "src-tauri/src/lib.rs",
    }
)


def _code_before_comment(line: str) -> str:
    """`line` with its comments and trailing whitespace removed.

    `} // end tests` is valid Rust that rustfmt preserves, so matching the
    bare shapes alone misses the terminator and swallows the rest of the file.

    A `/* ... */` span is closed and removed rather than treated as running to
    end of line, because it can be followed by real code: truncating at the
    opener turns `mod tests { /* todo */ }` into `mod tests {`, throwing away
    the brace that closes the body and losing the terminator the same way.

    Rust's block comments nest, so the span is tracked by depth rather than
    closed at the first `*/`: `} /* a /* b */ */` is one comment and a brace,
    and stopping at the inner `*/` leaves `}   */`, which is not a terminator
    either. rustfmt keeps that line as written, unlike `mod tests { /* todo */
    }` which it breaks in two, so it is a shape this tree can actually hold.

    This is applied to every line, not only to the shapes matched on. That is
    safe for the two callers comparing the result to a constant, and it is why
    the third — the self-contained check, which reads the result as content —
    needs the spans removed accurately rather than approximately. A `//` or
    `/*` inside a string literal would be mangled here; that costs nothing on
    the shapes actually compared, and a raw string holding a column-0 `}` only
    ever ends a block early, which counts more lines rather than fewer.
    """
    kept: list[str] = []
    depth = 0
    index = 0
    while index < len(line):
        if line.startswith("/*", index):
            depth += 1
            index += 2
        elif depth and line.startswith("*/", index):
            depth -= 1
            index += 2
        elif depth:
            index += 1
        elif line.startswith("//", index):
            break  # line comment: the rest really does run to end of line.
        else:
            kept.append(line[index])
            index += 1
    return "".join(kept).rstrip()


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
      * A declaration line can close its own block. rustfmt collapses an empty
        body onto it, so `mod tests {}` is the only spelling `cargo fmt` will
        emit (compare `control/server.rs`, which has `pub fn cleanup() {}` at
        column 0). Scanning below it for a `}` finds the *next* item's brace,
        or none, and swallows the file: it scores 0.
      * A matched line can carry a comment. `} // end tests` is valid Rust and
        rustfmt keeps it, so an exact `}` match walks past the real terminator
        to the next one and swallows everything between: another 0. A closed
        `/* ... */` span can likewise be followed by the brace that matters.

    Finding the end of a block by the next column-0 `}` leans on rustfmt
    putting the closing brace of a top-level item at column 0, which holds
    because `cargo fmt --check` is a blocking gate here (CONTRIBUTING.md). It
    is a line-based heuristic, not a Rust parser, which is proportionate for a
    navigability guardrail.

    Where the heuristic cannot be sure, it counts lines as code, so a
    surprising input reports a file as too big rather than waving it through.
    A nested (indented) test module counts as code, which overcounts
    platform/mod.rs by the ~560 lines of its two inner test modules; so does a
    block whose closing brace never arrives. There is no general "never
    undercounts" guarantee to lean on here, and an earlier version of this
    docstring claimed one: it was written from the nested case, and both the
    mid-file and `{}` shapes above broke it. Add a test before adding a claim.
    """
    lines = source.splitlines()
    total = 0
    index = 0
    while index < len(lines):
        # No lstrip anywhere here: a leading space means the item is nested
        # inside another, and only top-level test modules are skipped.
        if _code_before_comment(lines[index]) == "#[cfg(test)]":
            declaration = index + 1
            while declaration < len(lines) and not lines[declaration].strip():
                declaration += 1
            if declaration < len(lines) and lines[declaration].startswith("mod "):
                # `mod tests;` (body in another file) or `mod tests {}` (empty,
                # the form rustfmt emits): self-contained, no body to skip.
                if _code_before_comment(lines[declaration]).endswith((";", "}")):
                    index = declaration + 1
                    continue
                end = declaration + 1
                while end < len(lines) and _code_before_comment(lines[end]) != "}":
                    end += 1
                if end < len(lines):
                    index = end + 1
                    continue
                # Ran off the end without finding the brace. Count the rest as
                # code rather than silently swallowing it; an over-cap file
                # must not pass because its formatting confused the scanner.
        total += 1
        index += 1
    return total


def tracked_rust_files(root: Path) -> list[str]:
    """List the tracked .rs files under src-tauri/src, via git.

    `git ls-files` rather than a filesystem walk, deliberately: the bundled
    Python tree carries 20k-line vendored sources and a working tree may hold
    an untracked nested checkout of this repo. Neither is ours to measure and
    neither is tracked.

    One pathspec, not two: in a default pathspec git's `*` matches across `/`,
    so this already covers nested files. (It is `:(glob)` magic that sets
    FNM_PATHNAME and stops that; `src-tauri/src/**/*.rs` would match only the
    14 nested files and miss lib.rs, main.rs and dialog.rs.)

    Raises RuntimeError rather than returning an empty list or a bare
    traceback, because every way this can go wrong is a way for the gate to
    pass while measuring nothing.
    """
    try:
        completed = subprocess.run(
            ["git", "ls-files", "-z", "src-tauri/src/*.rs"],
            cwd=root,
            check=True,
            capture_output=True,
            text=True,
        )
    except subprocess.CalledProcessError as error:
        # capture_output hides git's stderr, and CalledProcessError does not
        # include it in its message, so without this a failure here (not a
        # repo, a bad pathspec) reaches the log as a bare traceback saying
        # only that the exit status was non-zero.
        raise RuntimeError(
            f"git ls-files failed ({error.returncode}): {error.stderr.strip()}"
        ) from error
    except OSError as error:
        # A missing git raises FileNotFoundError from the exec, never
        # CalledProcessError, so it does not go through the branch above.
        raise RuntimeError(f"could not run git: {error}") from error

    files = sorted(entry for entry in completed.stdout.split("\0") if entry)
    if not files:
        # `git ls-files` exits 0 and prints nothing when a pathspec matches
        # nothing; only --error-unmatch makes it fail. So if src-tauri/src is
        # renamed, the gate would scan zero files and pass, which is the exact
        # thing this check exists to prevent (see lint-test.yml on #325: a
        # check that is green whether or not it passes reads as coverage while
        # providing none). Stale EXEMPT entries would mask it today, but EXEMPT
        # is meant to shrink to nothing.
        raise RuntimeError(
            "git ls-files matched no .rs files under src-tauri/src. Either the "
            "tree moved and this script's pathspec needs updating, or this is "
            "not the repo root."
        )
    return files


def check(root: Path, files: Iterable[str], exempt: Iterable[str]) -> list[str]:
    """Return a message for every violation; empty means the tree is clean."""
    exempt = set(exempt)
    failures: list[str] = []
    seen: set[str] = set()

    for relative in files:
        try:
            source = (root / relative).read_text(encoding="utf-8")
        except FileNotFoundError as error:
            # git ls-files reads the index, so it lists a file deleted from the
            # working tree but not staged. CI is a fresh checkout and
            # pre-commit stashes, so this only reaches someone running the
            # script by hand in a dirty tree — which is what CONTRIBUTING.md
            # tells them to do.
            raise RuntimeError(
                f"{relative} is tracked but missing from the working tree; "
                f"`git status` should say why."
            ) from error
        count = code_line_count(source)
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
                f"policies). The cap does not count top-level #[cfg(test)] mod "
                f"blocks, wherever in the file they sit."
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
    try:
        failures = check(root, tracked_rust_files(root), EXEMPT)
    except RuntimeError as error:
        # Those raises exist to replace a traceback with a sentence; letting
        # one escape here would deliver the sentence wrapped in the traceback.
        print(f"error: {error}", file=sys.stderr)
        return 1
    for failure in failures:
        print(f"error: {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
