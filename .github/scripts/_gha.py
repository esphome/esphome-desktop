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


def emit_outputs(**outputs: str) -> None:
    """Write step outputs to $GITHUB_OUTPUT (or stdout when run locally)."""
    lines: list[str] = []
    for key, value in outputs.items():
        if "\n" in value:
            delimiter = f"__GHA_EOF_{key.upper()}__"
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
