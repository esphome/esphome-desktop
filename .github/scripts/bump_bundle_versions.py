#!/usr/bin/env python3
"""Bump pinned bundled dependencies in build-scripts/prepare_bundle.sh.

Run nightly by .github/workflows/bump-bundle-versions.yml, once per target
(`--target python` and `--target mingit`). Each resolves the latest upstream
release, rewrites the pinned values in prepare_bundle.sh, and emits GitHub
Actions outputs that the workflow turns into its own pull request.

Python policy: PBS_VERSION (the build-date tag) always moves to the latest
release; PYTHON_VERSION only takes a newer *patch* within the currently pinned
minor (e.g. 3.13.x), never a minor or major jump, which could break
ESPHome/PlatformIO. A deliberate minor bump stays a manual change.

MinGit policy: always track the latest git-for-windows release. Git is invoked
as a subprocess, so a new minor or major won't break ESPHome/PlatformIO the way
a Python minor could, and Git for Windows only maintains the newest line.

The script fails loudly (non-zero exit) if the expected variables aren't found
in prepare_bundle.sh, or if the latest upstream release can't be resolved —
both mean a broken assumption, and a silent no-op would let the bundled Python
drift and ship a stale, vulnerable version unnoticed. The only routine no-op is
"already up to date".

The version-rewriting transforms are pure and unit-tested
(tests/test_bump_bundle_versions.py); the network resolution lives in
resolve_latest_python, exercised against the real upstream API when the
workflow runs.

Usage (GITHUB_TOKEN keeps the API calls off the anonymous rate limit):

    GITHUB_TOKEN=... python3 .github/scripts/bump_bundle_versions.py --target python
    GITHUB_TOKEN=... python3 .github/scripts/bump_bundle_versions.py --target mingit
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

GFW_REPO = "git-for-windows/git"

# The plain 64-bit MinGit zip — not busybox, arm64 or 32-bit. The version token
# is digits/dots only, so MinGit-2.54.0-busybox-64-bit.zip can't match while
# rebuilds like MinGit-2.53.0.3-64-bit.zip still do.
MINGIT_ASSET_RE = re.compile(r"MinGit-(?P<ver>[\d.]+)-64-bit\.zip")

# Per-request network timeout. A stalled connection otherwise blocks the nightly
# job until the workflow's multi-hour default timeout fires; failing fast lets
# the run fail promptly (after retries) and surface a clear error.
HTTP_TIMEOUT = 30

# The Linux x86_64 asset is used to enumerate the CPython patch releases present
# in a python-build-standalone release; it's the canonical name shape and every
# release ships it. `{minor}` is filled in with the currently pinned minor.
PBS_ASSET_RE_TEMPLATE = (
    r"cpython-(?P<py>{minor}\.\d+)\+(?P<pbs>\d+)-"
    r"x86_64-unknown-linux-gnu-install_only_stripped\.tar\.gz"
)


def _warn(msg: str) -> None:
    """Emit a GitHub-Actions-style warning to stderr (also readable locally)."""
    print(f"::warning::{msg}", file=sys.stderr)


def _error(msg: str) -> None:
    """Emit a GitHub-Actions-style error to stderr (also readable locally)."""
    print(f"::error::{msg}", file=sys.stderr)


class ResolutionError(Exception):
    """An upstream assumption broke (missing asset, unresolvable release).

    Raised so the nightly job fails loudly rather than silently no-opping and
    letting a bundled dependency drift to a stale, vulnerable version.
    """


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


def _version_tuple(version: str) -> tuple[int, ...]:
    """Parse a dotted numeric version (e.g. "3.13.13") into an int tuple so
    versions order numerically rather than lexically ("3.13.9" < "3.13.10")."""
    return tuple(int(part) for part in version.split("."))


def is_downgrade(current: str, candidate: str) -> bool:
    """True when `candidate` is an older CPython release than `current`.

    `resolve_latest_python` returns the highest patch present in the *latest*
    python-build-standalone release. That release is normally a superset of
    older ones, but if it ever ships without the currently pinned patch (a
    yanked or partially-rebuilt release), the resolver would hand back a lower
    patch. Applying it would silently downgrade the bundled interpreter —
    losing whatever the newer patch fixed, often a security release — via an
    automated PR. PYTHON_VERSION and PBS_VERSION are coupled into the asset
    download URL, so the newer build tag can't keep the higher patch either;
    the only safe move is to refuse and fail loudly for a human to investigate.
    """
    return _version_tuple(candidate) < _version_tuple(current)


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
    highest `minor.patch` CPython build it ships. Returns None if that release
    has no build for `minor` (e.g. the line was dropped upstream); callers
    should treat this as an error rather than jumping to a different minor.
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


def _download_sha256(url: str) -> str:
    """Stream a URL and return its SHA-256 hex digest."""

    def fetch() -> str:
        req = urllib.request.Request(url, headers=_gh_headers())
        digest = hashlib.sha256()
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:  # noqa: S310
            for chunk in iter(lambda: resp.read(1 << 20), b""):
                digest.update(chunk)
        return digest.hexdigest()

    return _with_retries(fetch)


def _asset_sha256(asset: dict[str, Any]) -> str:
    """SHA-256 hex of a release asset.

    Prefer the digest GitHub reports on the asset (no download); fall back to
    streaming the asset and hashing it if the API ever omits it. A wrong value
    can't ship: prepare_bundle.sh verifies the download at build time, so the
    Windows build goes red on mismatch rather than bundling the wrong bytes.
    """
    digest = asset.get("digest") or ""
    if digest.startswith("sha256:"):
        return digest.split(":", 1)[1]
    url = asset.get("browser_download_url")
    if not url:
        raise ResolutionError(
            f"asset {asset.get('name')!r} has no digest or download URL"
        )
    return _download_sha256(url)


def resolve_latest_mingit() -> tuple[str, str, str]:
    """Return `(version, url, sha256)` for the latest 64-bit MinGit zip.

    Picks the plain x86-64 MinGit asset from the latest git-for-windows release
    and reads its checksum, so the literal download URL and digest land in
    prepare_bundle.sh even for `.windows.N` rebuilds whose filename carries the
    build number.
    """
    release = _api_get(f"https://api.github.com/repos/{GFW_REPO}/releases/latest")
    for asset in release.get("assets", []):
        m = MINGIT_ASSET_RE.fullmatch(asset.get("name", ""))
        if m is not None:
            return m.group("ver"), asset["browser_download_url"], _asset_sha256(asset)
    raise ResolutionError(
        f"no 64-bit MinGit asset in {GFW_REPO} release {release.get('tag_name')!r}"
    )


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


def _build_body(subject: str, var_changes: dict[str, tuple[str, str]]) -> str:
    """Short, human-readable PR body describing the bump."""
    bullets = "\n".join(
        f"* {var} {old} to {new}" for var, (old, new) in var_changes.items()
    )
    return (
        f"Automated nightly bump of the bundled {subject}.\n\n"
        f"{bullets}\n\n"
        "CI builds and tests this branch; merge once the platform builds pass."
    )


def resolve_python_bump(text: str) -> tuple[BumpResult, str]:
    """Resolve the latest Python and compute the bump over `text`."""
    latest = resolve_latest_python(current_python_minor(text))
    if latest is None:
        # The pinned minor has no build in the latest release (resolver already
        # logged why). That's a broken assumption, not a routine skip.
        raise ResolutionError(
            "could not resolve the latest Python for the pinned minor"
        )

    pbs_version, python_version = latest

    # Refuse a patch regression. The latest PBS release should never ship a
    # lower CPython patch than we already pin, but if it does, downgrading the
    # bundled interpreter via an automated PR would silently undo a (likely
    # security) patch. Fail loudly instead — same philosophy as the
    # missing-variable / unresolvable-upstream paths above.
    current_python = read_assignment(text, "PYTHON_VERSION")
    if is_downgrade(current_python, python_version):
        _error(
            f"latest {PBS_REPO} release ships CPython {python_version}, older "
            f"than the pinned {current_python}; refusing to downgrade the "
            "bundled interpreter. Investigate upstream before bumping."
        )
        return 1

    result = apply_bumps(
        text, {"PYTHON_VERSION": python_version, "PBS_VERSION": pbs_version}
    )
    # When only the PBS build-date tag moves (same CPython patch), don't claim a
    # version bump that didn't happen; name the build instead.
    if "PYTHON_VERSION" in result.var_changes:
        title = f"Bump bundled Python to {python_version}"
    else:
        title = f"Bump bundled Python build to {pbs_version} ({python_version})"
    return result, title


def _version_tuple(version: str) -> tuple[int, ...]:
    """Parse a dotted MinGit version (e.g. "2.54.0" or rebuild "2.53.0.3")."""
    return tuple(int(part) for part in version.split("."))


def resolve_mingit_bump(text: str) -> tuple[BumpResult, str]:
    """Resolve the latest MinGit and compute the bump over `text`."""
    current = read_assignment(text, "MINGIT_VERSION")
    version, url, sha256 = resolve_latest_mingit()
    # Git for Windows releases move forward, so a resolved version older than the
    # current pin means upstream republished an old tag as `latest` or unpublished
    # a release. That's a broken assumption, not a routine skip: fail loudly
    # rather than open a PR moving bundled git backwards (the sha check would pass
    # on a genuine older release, so only this guard catches it).
    if _version_tuple(version) < _version_tuple(current):
        raise ResolutionError(
            f"latest MinGit {version} is older than the pinned {current}; "
            "refusing to downgrade"
        )
    result = apply_bumps(
        text,
        {"MINGIT_VERSION": version, "MINGIT_URL": url, "MINGIT_SHA256": sha256},
    )
    return result, f"Bump bundled MinGit to {version}"


# Resolves the latest upstream release and computes the bump over the file text.
_Resolver = Callable[[str], tuple[BumpResult, str]]

# Each target names the prepare_bundle.sh variables that must exist (a missing
# one means the file was restructured and this script needs updating), the
# resolver that computes its bump, and the noun used in the PR body.
TARGETS: dict[str, tuple[tuple[str, ...], _Resolver, str]] = {
    "python": (
        ("PYTHON_VERSION", "PBS_VERSION"),
        resolve_python_bump,
        "Python interpreter",
    ),
    "mingit": (
        ("MINGIT_VERSION", "MINGIT_URL", "MINGIT_SHA256"),
        resolve_mingit_bump,
        "MinGit (Git for Windows)",
    ),
}


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target",
        choices=sorted(TARGETS),
        default="python",
        help="Which bundled dependency to bump.",
    )
    parser.add_argument(
        "--file",
        default=str(PREPARE_BUNDLE),
        help="Path to prepare_bundle.sh (defaults to the in-repo copy).",
    )
    args = parser.parse_args(argv)

    required, resolve_bump, subject = TARGETS[args.target]

    path = Path(args.file)
    text = path.read_text(encoding="utf-8")

    # These variables are expected to exist. A missing one means
    # prepare_bundle.sh was renamed/restructured and this script needs
    # updating, so fail loudly rather than silently reporting "nothing to bump"
    # — a quiet no-op would let the bundled dependency drift and could ship a
    # stale, vulnerable version unnoticed.
    missing = [var for var in required if not has_assignment(text, var)]
    if missing:
        _error(
            f"{', '.join(missing)} not found in {path.name}; "
            "the bump script needs updating"
        )
        return 1

    try:
        result, title = resolve_bump(text)
    except ResolutionError as exc:
        # A broken upstream assumption, not a routine skip: fail loudly rather
        # than silently never bumping.
        _error(str(exc))
        return 1

    if not result.changed:
        print(f"Bundled {subject} already up to date; nothing to bump.")
        _emit_outputs(changed="false")
        return 0

    path.write_text(result.text, encoding="utf-8")
    print(f"Bumped bundled {subject}:")
    for var, (old, new) in result.var_changes.items():
        print(f"  {var}: {old} -> {new}")

    _emit_outputs(
        changed="true",
        title=title,
        body=_build_body(subject, result.var_changes),
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
