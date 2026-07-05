#!/usr/bin/env python3
"""Shared GitHub Actions annotation helpers for the release scripts.

The sibling scripts in this directory run by path (they aren't an installed
package), so each one puts this directory on sys.path before importing.
"""

from __future__ import annotations

import sys


def warn(msg: str) -> None:
    """Emit a GitHub-Actions-style warning to stderr (also readable locally)."""
    print(f"::warning::{msg}", file=sys.stderr)


def error(msg: str) -> None:
    """Emit a GitHub-Actions-style error to stderr (also readable locally)."""
    print(f"::error::{msg}", file=sys.stderr)
