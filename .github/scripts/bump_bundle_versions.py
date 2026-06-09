#!/usr/bin/env python3
"""Bump the pinned bundle dependency versions in build-scripts/prepare_bundle.sh.

Run nightly by .github/workflows/bump-bundle-versions.yml, once per dependency.
Each run resolves the latest upstream release, rewrites the pinned version (and,
for MinGit, its SHA-256) in prepare_bundle.sh, and emits GitHub Actions outputs
that the workflow turns into a pull request.

Policy:
  * python  tracks python-build-standalone. PBS_VERSION (the build-date tag)
            always moves to the latest release; PYTHON_VERSION only takes a
            newer *patch* within the currently pinned minor (e.g. 3.13.x),
            never a minor or major jump, which could break ESPHome/PlatformIO.
            A deliberate minor bump stays a manual change.
  * mingit  tracks git-for-windows. Takes the latest release's MinGit 64-bit
            zip and repins MINGIT_VERSION + MINGIT_SHA256.

If a dependency's variables aren't present in prepare_bundle.sh yet (e.g. the
MinGit support hasn't merged), the run is a quiet no-op rather than an error.

The version-rewriting transforms are pure and unit-tested
(tests/test_bump_bundle_versions.py); the network resolution lives in the
resolve_* functions, exercised against the real upstream APIs when the workflow
runs.

Usage (GITHUB_TOKEN keeps the API calls off the anonymous rate limit):

    GITHUB_TOKEN=... python3 .github/scripts/bump_bundle_versions.py \
        --dependency python|mingit
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, TypeVar

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
PREPARE_BUNDLE = REPO_ROOT / "build-scripts" / "prepare_bundle.sh"

PBS_REPO = "astral-sh/python-build-standalone"
GIT_FOR_WINDOWS_REPO = "git-for-windows/git"

# Per-request network timeout. A stalled connection otherwise blocks the nightly
# job until the workflow's multi-hour default timeout fires; failing fast lets
# the run report a clean no-op instead.
HTTP_TIMEOUT = 30

# The Linux x86_64 asset is used to enumerate the CPython patch releases present
# in a python-build-standalone release; it's the canonical name shape and every
# release ships it. `{minor}` is filled in with the currently pinned minor.
PBS_ASSET_RE_TEMPLATE = (
    r"cpython-(?P<py>{minor}\.\d+)\+(?P<pbs>\d+)-"
    r"x86_64-unknown-linux-gnu-install_only_stripped\.tar\.gz"
)

# git-for-windows tags look like `v2.54.0.windows.1`; the MinGit asset drops the
# `.windows.N` suffix (`MinGit-2.54.0-64-bit.zip`).
GIT_FOR_WINDOWS_TAG_RE = re.compile(r"v(?P<version>\d+\.\d+\.\d+)\.windows\.\d+")


def _warn(msg: str) -> None:
    """Emit a GitHub-Actions-style warning to stderr (also readable locally)."""
    print(f"::warning::{msg}", file=sys.stderr)


# --------------------------------------------------------------------------- #
# Pure transforms over the prepare_bundle.sh text (unit-tested).
# --------------------------------------------------------------------------- #


@dataclass
class BumpResult:
    """Outcome of applying a bump to the prepare_bundle.sh text.

    `changed` is true when at least one variable's value actually moved.
    `var_changes` maps each changed variable to its (old, new) pair, used to
    build the PR body. `text` is the rewritten file content.
    """

    changed: bool
    text: str
    var_changes: dict[str, tuple[str, str]] = field(default_factory=dict)


def _assignment_re(var: str) -> re.Pattern[str]:
    # Matches a top-level `VAR="value"` assignment, capturing the quoted value.
    return re.compile(rf'^({re.escape(var)}=")([^"]*)(")', re.MULTILINE)


def has_assignment(text: str, var: str) -> bool:
    return _assignment_re(var).search(text) is not None


def read_assignment(text: str, var: str) -> str:
    m = _assignment_re(var).search(text)
    if m is None:
        raise KeyError(var)
    return m.group(2)


def replace_assignment(text: str, var: str, value: str) -> str:
    pattern = _assignment_re(var)
    if pattern.search(text) is None:
        raise KeyError(var)
    # Function replacement so any backslashes/specials in `value` stay literal.
    return pattern.sub(lambda m: f"{m.group(1)}{value}{m.group(3)}", text, count=1)


def apply_bumps(text: str, updates: dict[str, str]) -> BumpResult:
    """Apply `var -> new value` updates, recording only the ones that move."""
    new = text
    changes: dict[str, tuple[str, str]] = {}
    for var, value in updates.items():
        old = read_assignment(new, var)
        if old != value:
            new = replace_assignment(new, var, value)
            changes[var] = (old, value)
    return BumpResult(changed=bool(changes), text=new, var_changes=changes)


def current_python_minor(text: str) -> str:
    """Return the `major.minor` of the currently pinned PYTHON_VERSION."""
    pinned = read_assignment(text, "PYTHON_VERSION")
    m = re.match(r"(\d+\.\d+)\.\d+$", pinned)
    if m is None:
        raise ValueError(f"Unexpected PYTHON_VERSION format: {pinned!r}")
    return m.group(1)


# --------------------------------------------------------------------------- #
# Upstream resolution (network).
# --------------------------------------------------------------------------- #


_T = TypeVar("_T")

# How many times to attempt a network op before giving up, and the linear
# backoff base between attempts.
HTTP_RETRIES = 3
HTTP_RETRY_BACKOFF = 2


def _with_retries(op: Callable[[], _T]) -> _T:
    """Run `op`, retrying transient network failures with linear backoff.

    We deliberately do NOT swallow a persistent failure into a no-op: if the
    job can't reach GitHub it should fail loudly (a red nightly run) so the bump
    is noticed and fixed. The opposite — quietly reporting "nothing to bump" on
    every error — is the worse failure mode, because the bundled dependency
    would silently drift and could ship a stale, vulnerable version for a long
    time before anyone noticed. Retries only paper over momentary blips so a
    single transient 5xx/timeout doesn't raise a false alarm.
    """
    last_exc: Exception | None = None
    for attempt in range(1, HTTP_RETRIES + 1):
        try:
            return op()
        except (urllib.error.URLError, TimeoutError, ConnectionError) as exc:
            last_exc = exc
            _warn(f"network attempt {attempt}/{HTTP_RETRIES} failed: {exc}")
            if attempt < HTTP_RETRIES:
                time.sleep(HTTP_RETRY_BACKOFF * attempt)
    assert last_exc is not None  # loop ran at least once and didn't return
    raise last_exc


def _gh_headers() -> dict[str, str]:
    headers = {
        "Accept": "application/vnd.github+json",
        "User-Agent": "esphome-desktop-version-bump",
    }
    token = os.environ.get("GITHUB_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _api_get(url: str) -> Any:
    def fetch() -> Any:
        req = urllib.request.Request(url, headers=_gh_headers())
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:  # noqa: S310
            return json.load(resp)

    return _with_retries(fetch)


def resolve_latest_python(minor: str) -> tuple[str, str] | None:
    """Latest `(PBS_VERSION, PYTHON_VERSION)` for the given minor (e.g. "3.13").

    Returns the newest python-build-standalone release tag together with the
    highest `minor.patch` CPython build it ships. None if that release has no
    build for `minor` (e.g. the line was dropped upstream), so the caller skips
    the bump rather than jumping minors.
    """
    release = _api_get(f"https://api.github.com/repos/{PBS_REPO}/releases/latest")
    pbs_version = release["tag_name"]
    asset_re = re.compile(PBS_ASSET_RE_TEMPLATE.format(minor=re.escape(minor)))

    patches: list[tuple[int, ...]] = []
    for asset in release.get("assets", []):
        m = asset_re.fullmatch(asset.get("name", ""))
        if m is not None:
            patches.append(tuple(int(p) for p in m.group("py").split(".")))

    if not patches:
        _warn(f"No CPython {minor}.x build in {PBS_REPO} release {pbs_version}")
        return None

    best = max(patches)
    return pbs_version, ".".join(str(p) for p in best)


def resolve_latest_mingit() -> tuple[str, str] | None:
    """Latest `(MINGIT_VERSION, sha256)` for the MinGit 64-bit zip.

    None if the release tag doesn't match the expected shape or the 64-bit
    MinGit asset is missing, so the caller skips rather than pinning garbage.
    """
    release = _api_get(
        f"https://api.github.com/repos/{GIT_FOR_WINDOWS_REPO}/releases/latest"
    )
    tag = release.get("tag_name", "")
    m = GIT_FOR_WINDOWS_TAG_RE.fullmatch(tag)
    if m is None:
        _warn(f"Unexpected {GIT_FOR_WINDOWS_REPO} tag {tag!r}")
        return None
    version = m.group("version")

    asset_name = f"MinGit-{version}-64-bit.zip"
    asset = next(
        (a for a in release.get("assets", []) if a.get("name") == asset_name), None
    )
    if asset is None:
        _warn(f"Asset {asset_name} not found in {GIT_FOR_WINDOWS_REPO} {tag}")
        return None

    return version, _asset_sha256(asset)


def _asset_sha256(asset: dict[str, Any]) -> str:
    """SHA-256 hex for a release asset, preferring the API's digest field.

    GitHub exposes `digest` as `sha256:<hex>` for newer assets; when present we
    avoid downloading the ~40 MB zip and just trust GitHub's own hash. We fall
    back to streaming the download through hashlib for older assets.
    """
    digest = asset.get("digest") or ""
    if digest.startswith("sha256:"):
        return digest.split(":", 1)[1]

    def stream() -> str:
        req = urllib.request.Request(
            asset["browser_download_url"],
            headers={"User-Agent": "esphome-desktop-version-bump"},
        )
        h = hashlib.sha256()
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:  # noqa: S310
            for chunk in iter(lambda: resp.read(1 << 20), b""):
                h.update(chunk)
        return h.hexdigest()

    return _with_retries(stream)


# --------------------------------------------------------------------------- #
# Output + CLI glue.
# --------------------------------------------------------------------------- #


def _emit_outputs(**outputs: str) -> None:
    """Write step outputs to $GITHUB_OUTPUT (or stdout when run locally)."""
    lines: list[str] = []
    for key, value in outputs.items():
        if "\n" in value:
            delimiter = f"__BUMP_EOF_{key.upper()}__"
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


def _build_body(name: str, var_changes: dict[str, tuple[str, str]]) -> str:
    """Short, human-readable PR body describing the bump."""
    bullets = "\n".join(
        f"* {var} {old} to {new}" for var, (old, new) in var_changes.items()
    )
    return (
        f"Automated nightly bump of the bundled {name}.\n\n"
        f"{bullets}\n\n"
        "CI builds and tests this branch; merge once the platform builds pass."
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dependency", choices=["python", "mingit"], required=True)
    parser.add_argument(
        "--file",
        default=str(PREPARE_BUNDLE),
        help="Path to prepare_bundle.sh (defaults to the in-repo copy).",
    )
    args = parser.parse_args(argv)

    path = Path(args.file)
    text = path.read_text(encoding="utf-8")

    if args.dependency == "python":
        required = ("PYTHON_VERSION", "PBS_VERSION")
        name = "Python interpreter"
    else:
        required = ("MINGIT_VERSION", "MINGIT_SHA256")
        name = "MinGit"

    missing = [var for var in required if not has_assignment(text, var)]
    if missing:
        _warn(
            f"{', '.join(missing)} not present in {path.name}; "
            f"skipping {args.dependency} bump"
        )
        _emit_outputs(changed="false")
        return 0

    if args.dependency == "python":
        latest = resolve_latest_python(current_python_minor(text))
        if latest is None:
            _emit_outputs(changed="false")
            return 0
        pbs_version, python_version = latest
        result = apply_bumps(
            text, {"PYTHON_VERSION": python_version, "PBS_VERSION": pbs_version}
        )
        # When only the PBS build-date tag moves (same CPython patch), don't
        # claim a version bump that didn't happen; name the build instead.
        if "PYTHON_VERSION" in result.var_changes:
            title = f"Bump bundled Python to {python_version}"
        else:
            title = f"Bump bundled Python build to {pbs_version} ({python_version})"
    else:
        latest = resolve_latest_mingit()
        if latest is None:
            _emit_outputs(changed="false")
            return 0
        version, sha256 = latest
        result = apply_bumps(
            text, {"MINGIT_VERSION": version, "MINGIT_SHA256": sha256}
        )
        title = f"Bump bundled MinGit to {version}"

    if not result.changed:
        print(f"{name} already up to date; nothing to bump.")
        _emit_outputs(changed="false")
        return 0

    path.write_text(result.text, encoding="utf-8")
    print(f"Bumped {name}:")
    for var, (old, new) in result.var_changes.items():
        print(f"  {var}: {old} -> {new}")

    _emit_outputs(
        changed="true",
        title=title,
        body=_build_body(name, result.var_changes),
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
