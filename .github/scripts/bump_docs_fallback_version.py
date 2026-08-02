#!/usr/bin/env python3
"""Bump FALLBACK_VERSION in the esphome.io install page to a released version.

Run by .github/workflows/bump-docs-fallback-version.yml after a (non-pre)release
is published, against a checkout of esphome/esphome.io. That repo's install page
resolves the current Device Builder version by fetching latest.json at build
time and only falls back to this constant when the fetch fails (offline build,
GitHub outage, HTTP error, or a version that fails the page's own validation).
Nothing forces a fallback to stay current, so it drifted seven releases behind
before anyone noticed; this keeps it fresh automatically.

The rewrite is anchored on the `const FALLBACK_VERSION` declaration and must
match exactly once. A looser match on any version-shaped string would corrupt
the download URLs and console warnings that fill the rest of the file, and a
silent no-op would let the constant drift again unnoticed, so a match count
other than one fails the job loudly. That is also what happens if the install
page is renamed or restructured, which is the signal that this script needs
updating.

The pull request body is esphome.io's own PULL_REQUEST_TEMPLATE.md, read from the
same checkout and filled in (description inserted, `current` box ticked), rather
than a copy kept here that would drift from the upstream template.

The transforms are pure and unit-tested (tests/test_bump_docs_fallback_version.py).

Usage:

    python3 .github/scripts/bump_docs_fallback_version.py 1.2.3 \
        --file esphome.io/src/components/InstallSelector.astro \
        --template esphome.io/.github/PULL_REQUEST_TEMPLATE.md
"""

from __future__ import annotations

import argparse
import re
import sys
from collections.abc import Callable
from pathlib import Path

# This script runs by path (not as an installed package), so make the sibling
# _gha helper importable regardless of the caller's cwd.
_SCRIPT_DIR = str(Path(__file__).resolve().parent)
if _SCRIPT_DIR not in sys.path:
    sys.path.insert(0, _SCRIPT_DIR)

from _gha import emit_outputs as _emit_outputs  # noqa: E402
from _gha import error as _error  # noqa: E402
from _gha import warn as _warn  # noqa: E402

# Path of the install page inside an esphome/esphome.io checkout.
INSTALL_SELECTOR = "src/components/InstallSelector.astro"

# Mirrors normalizeVersion() in the install page. The page rejects anything else
# and falls back, so writing such a value would leave the fallback pointing at a
# version the page itself refuses to use.
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$")

# Anchored on the declaration line, capturing the quoted value. FALLBACK_VERSION
# is referenced five more times in the file (in the warning messages), so the
# `const ... = "` prefix is what makes this a single match.
FALLBACK_RE = re.compile(r'^(const FALLBACK_VERSION = ")([^"]*)(";)$', re.MULTILINE)

RELEASES_URL = "https://github.com/esphome/esphome-desktop/releases/tag/v{version}"

# Path of esphome.io's own pull request template inside its checkout. The body is
# built by filling that in rather than by keeping a copy here, so an upstream
# edit to the template flows straight through instead of silently drifting.
PR_TEMPLATE = ".github/PULL_REQUEST_TEMPLATE.md"

# The heading the description is inserted under, and the `current` checkbox. The
# checkbox lookahead is what keeps the tick off the `next` line directly above
# it, which is otherwise identical up to the wording.
DESCRIPTION_HEADING_RE = re.compile(r"^## Description$", re.MULTILINE)
CURRENT_CHECKBOX_RE = re.compile(
    r"^- \[ \](?= I am merging into `current`)", re.MULTILINE
)

# The template's two "if applicable" placeholders, neither of which applies to an
# automated bump. Left alone they read badly: GitHub swallows `<link to issue>`
# and `<esphome PR number goes here>` as unknown HTML tags, rendering a dangling
# "fixes" and a bare "esphome/esphome#".
RELATED_ISSUE_RE = re.compile(
    r"^(\*\*Related issue \(if applicable\):\*\*).*$", re.MULTILINE
)
ESPHOME_PR_RE = re.compile(r"^- esphome/esphome#.*$", re.MULTILINE)

# One line per paragraph: GitHub wraps prose, and hard-wrapping it here would
# show up as ragged line breaks in the rendered pull request.
DESCRIPTION = (
    "Automated bump of `FALLBACK_VERSION` in `{install_selector}` from "
    "`{previous}` to `{version}`, following the publication of "
    "[ESPHome Device Builder v{version}]({release_url}).\n"
    "\n"
    "The install page resolves the current version by fetching `latest.json` at "
    "build time; this constant is only used when that fetch fails (offline "
    "build, GitHub outage, HTTP error, or a version that fails validation). "
    "Keeping it current means a failed fetch degrades to the newest release "
    "rather than to whatever happened to be current when the constant was last "
    "touched by hand."
)


def _sub_once(
    pattern: re.Pattern[str],
    repl: Callable[[re.Match[str]], str],
    text: str,
    what: str,
) -> str:
    """Apply `repl` to the single match of `pattern`, or raise ValueError.

    A match count other than one means the file this pattern anchors on was
    renamed or restructured. Failing here turns that into a red job, where a
    silent no-op would let the fallback drift again unnoticed (or open a pull
    request with an empty description and no box ticked).

    `repl` is a function so any backslashes or specials in the replacement stay
    literal rather than being read as group references.
    """
    new, count = pattern.subn(repl, text)
    if count != 1:
        raise ValueError(f"{what} matched {count} times, expected exactly 1")
    return new


def _sub_once_optional(
    pattern: re.Pattern[str],
    repl: Callable[[re.Match[str]], str],
    text: str,
    what: str,
) -> str:
    """Like `_sub_once`, but a miss only warns and leaves the text alone.

    For edits that are cosmetic rather than load-bearing. An unticked merge-target
    checkbox is wrong; an untidied "if applicable" placeholder is only untidy, and
    should not fail a release-triggered bump because upstream reworded a line.
    """
    try:
        return _sub_once(pattern, repl, text, what)
    except ValueError as exc:
        _warn(f"{exc}; leaving it as the template has it")
        return text


def bump_fallback_version(text: str, version: str) -> tuple[str, str]:
    """Rewrite the FALLBACK_VERSION declaration, returning (new_text, previous).

    Raises ValueError when the declaration is not present exactly once, so a
    renamed or restructured install page fails the job instead of silently
    leaving a stale fallback in place, and an over-broad match fails instead of
    rewriting unrelated content.
    """
    matches = FALLBACK_RE.findall(text)
    if len(matches) != 1:
        raise ValueError(
            f"const FALLBACK_VERSION declaration matched {len(matches)} times, "
            "expected exactly 1"
        )
    previous = matches[0][1]
    new = _sub_once(
        FALLBACK_RE,
        lambda m: f"{m.group(1)}{version}{m.group(3)}",
        text,
        "const FALLBACK_VERSION declaration",
    )
    return new, previous


def build_title(version: str) -> str:
    """PR title and commit message, in esphome.io's `[component] Summary` style."""
    return f"[install] Bump Device Builder fallback version to {version}"


def build_body(template: str, previous: str, version: str) -> str:
    """Fill in esphome.io's own pull request template.

    Four anchored edits. The description is inserted under its heading and the
    `current` checkbox is ticked - `current` is the right box because this is an
    adjustment to existing documentation, not docs for a new component, so it
    does not belong on `next`. Both are load-bearing, and raise ValueError if
    their anchor is not present exactly once, so an upstream restructure of the
    template fails the job rather than opening a pull request with an empty
    description or no box ticked.

    The other two replace the "if applicable" placeholders with N/A, neither
    being applicable to an automated bump. Those are cosmetic, so a miss warns
    instead of failing.
    """
    description = DESCRIPTION.format(
        install_selector=INSTALL_SELECTOR,
        previous=previous,
        version=version,
        release_url=RELEASES_URL.format(version=version),
    )
    body = _sub_once(
        DESCRIPTION_HEADING_RE,
        lambda m: f"{m.group(0)}\n\n{description}",
        template,
        "'## Description' heading",
    )
    body = _sub_once(
        CURRENT_CHECKBOX_RE,
        lambda _m: "- [x]",
        body,
        "'merging into `current`' checkbox",
    )
    body = _sub_once_optional(
        RELATED_ISSUE_RE,
        lambda m: f"{m.group(1)} N/A",
        body,
        "'Related issue' placeholder",
    )
    return _sub_once_optional(
        ESPHOME_PR_RE,
        lambda _m: "- N/A",
        body,
        "'esphome/esphome#' placeholder",
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "version",
        help="Released version, without the leading v (e.g. 1.2.3).",
    )
    parser.add_argument(
        "--file",
        default=INSTALL_SELECTOR,
        help="Path to InstallSelector.astro in the esphome.io checkout.",
    )
    parser.add_argument(
        "--template",
        default=PR_TEMPLATE,
        help="Path to PULL_REQUEST_TEMPLATE.md in the esphome.io checkout.",
    )
    args = parser.parse_args(argv)

    version = args.version.strip()
    if not VERSION_RE.match(version):
        _error(
            f"{version!r} is not a valid version; the install page would reject "
            "it and fall back anyway"
        )
        return 1

    path = Path(args.file)
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as exc:
        _error(f"{path}: {exc}")
        return 1

    try:
        new, previous = bump_fallback_version(text, version)
    except ValueError as exc:
        _error(
            f"{path}: {exc}; the install page changed and this script needs updating"
        )
        return 1

    # The only routine no-op: a re-run, or a release whose bump already landed.
    if previous == version:
        print(f"FALLBACK_VERSION is already {version}; nothing to bump.")
        _emit_outputs(changed="false")
        return 0

    # Build the body before touching the install page, so a missing or
    # restructured template fails with the checkout still pristine rather than
    # leaving a bumped file behind that no pull request will ever carry.
    template_path = Path(args.template)
    try:
        body = build_body(template_path.read_text(encoding="utf-8"), previous, version)
    except OSError as exc:
        _error(f"{template_path}: {exc}")
        return 1
    except ValueError as exc:
        _error(
            f"{template_path}: {exc}; esphome.io's pull request template changed "
            "and this script needs updating"
        )
        return 1

    try:
        path.write_text(new, encoding="utf-8")
    except OSError as exc:
        _error(f"{path}: {exc}")
        return 1

    print(f"FALLBACK_VERSION: {previous} -> {version}")
    _emit_outputs(changed="true", title=build_title(version), body=body)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
