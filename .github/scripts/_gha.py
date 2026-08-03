#!/usr/bin/env python3
"""Shared GitHub Actions annotation and step-output helpers for the release scripts.

The sibling scripts in this directory run by path (they aren't an installed
package), so each one puts this directory on sys.path before importing.
"""

from __future__ import annotations

import os
import sys


def _escape(msg: str) -> str:
    """Escape workflow-command message data per GitHub's escaping rules."""
    return msg.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def warn(msg: str) -> None:
    """Emit a GitHub-Actions-style warning to stderr (also readable locally)."""
    print(f"::warning::{_escape(msg)}", file=sys.stderr)


def error(msg: str) -> None:
    """Emit a GitHub-Actions-style error to stderr (also readable locally)."""
    print(f"::error::{_escape(msg)}", file=sys.stderr)


def _heredoc_delimiter(key: str, value: str) -> str:
    """Pick a $GITHUB_OUTPUT heredoc delimiter that cannot occur inside `value`.

    GitHub ends the value at the first line equal to the delimiter, so a value
    that happens to contain the delimiter on its own line would be silently
    truncated - and anything after it read as further step outputs, which is how
    a value sourced from outside the repo turns into forged outputs. Widen the
    delimiter until no line matches; the value is finite, so this terminates.
    """
    base = f"__GHA_EOF_{key.upper()}__"
    value_lines = set(value.split("\n"))
    delimiter = base
    suffix = 0
    while delimiter in value_lines:
        suffix += 1
        delimiter = f"{base}{suffix}__"
    return delimiter


def emit_outputs(**outputs: str) -> None:
    """Write step outputs to $GITHUB_OUTPUT (or stdout when run locally)."""
    lines: list[str] = []
    for key, value in outputs.items():
        if "\n" in value:
            delimiter = _heredoc_delimiter(key, value)
            lines += [f"{key}<<{delimiter}", value, delimiter]
        else:
            lines.append(f"{key}={value}")
    payload = "\n".join(lines) + "\n"

    target = os.environ.get("GITHUB_OUTPUT")
    if target:
        with open(target, "a", encoding="utf-8") as fh:
            fh.write(payload)
    else:
        sys.stdout.write(payload)
