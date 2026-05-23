#!/usr/bin/env python3
"""Generate the latest.json release manifest.

Invoked from the release job in .github/workflows/build.yml after build
artifacts have been attached to the draft release. Produces a JSON manifest
with:

  * `platforms` — Tauri updater bundles + signatures (consumed by
    tauri-plugin-updater for in-app self-updates).
  * `downloads` — every distributable installer URL grouped by platform,
    including formats the updater doesn't ship (.deb, .rpm, .dmg). For
    download pages and other consumers.
  * `release_url`, `pub_date`, `notes` — release metadata.

The manifest is uploaded as a release asset and mirrored to
https://desktop.esphome.io/latest.json by the deploy-pages workflow.

Usage in CI (TAG / REPO / GH_TOKEN from env):

    python3 .github/scripts/generate_latest_json.py

Usage for local testing — run against the in-repo fixtures, no network
and no `gh` auth required:

    python3 .github/scripts/generate_latest_json.py \
        --tag v0.10.0 \
        --repo esphome/esphome-desktop \
        --release-fixture tests/fixtures/release.json \
        --artifacts-dir tests/fixtures/artifacts \
        --output /tmp/latest.json

To exercise a real release's data shape, replace the fixture with
`gh release view <tag> --json body,publishedAt,assets > my-release.json`.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


# (Tauri updater target, regex matching the .sig asset name)
PLATFORM_SIG_MATCHERS: list[tuple[str, re.Pattern[str]]] = [
    ("windows-x86_64", re.compile(r".+-setup\.exe\.sig$")),
    ("linux-x86_64",   re.compile(r".+_amd64\.AppImage\.sig$")),
    ("linux-aarch64",  re.compile(r".+_aarch64\.AppImage\.sig$")),
    ("darwin-aarch64", re.compile(r".+_aarch64\.app\.tar\.gz\.sig$")),
    ("darwin-x86_64",  re.compile(r".+_x64\.app\.tar\.gz\.sig$")),
]

# (Tauri updater target, canonical installer kind, regex matching asset name)
# The .app.tar.gz updater bundles are intentionally absent — they're only
# useful to the Tauri updater (already covered by `platforms`); a human
# downloading first-install on macOS wants the .dmg.
DOWNLOAD_MATCHERS: list[tuple[str, str, re.Pattern[str]]] = [
    ("windows-x86_64", "nsis",     re.compile(r"-setup\.exe$")),
    ("linux-x86_64",   "appimage", re.compile(r"_amd64\.AppImage$")),
    ("linux-aarch64",  "appimage", re.compile(r"_aarch64\.AppImage$")),
    ("linux-x86_64",   "deb",      re.compile(r"_amd64\.deb$")),
    ("linux-aarch64",  "deb",      re.compile(r"_arm64\.deb$")),
    ("linux-x86_64",   "rpm",      re.compile(r"\.x86_64\.rpm$")),
    ("linux-aarch64",  "rpm",      re.compile(r"\.aarch64\.rpm$")),
    ("darwin-aarch64", "dmg",      re.compile(r"_aarch64\.dmg$")),
    ("darwin-x86_64",  "dmg",      re.compile(r"_x64\.dmg$")),
]

SCHEMA_URL = "https://desktop.esphome.io/latest.schema.json"

# Matches the assets-pending caution block emitted by release-drafter
# (see .github/release-drafter.yml). The release body still carries this
# block while latest.json is generated because the workflow only strips
# it from the release body *after* the manifest is uploaded — so we
# strip it here too, to keep the warning out of the published notes.
ASSETS_PENDING_BLOCK_RE = re.compile(
    r"^> \[!CAUTION\].*?<!-- ASSETS_PENDING -->\n?",
    re.DOTALL | re.MULTILINE,
)


def _warn(msg: str) -> None:
    """Emit a GitHub-Actions-style warning to stderr (also readable locally)."""
    print(f"::warning::{msg}", file=sys.stderr)


def _strip_assets_pending_warning(body: str) -> str:
    """Remove the release-drafter assets-pending caution block from a release body."""
    stripped = ASSETS_PENDING_BLOCK_RE.sub("", body)
    # Belt-and-braces: drop any stray markers left behind by manual edits.
    stripped = stripped.replace("<!-- ASSETS_PENDING -->", "")
    return stripped.lstrip("\n")


def _asset_url(download_base: str, name: str) -> str:
    """Build the post-publish download URL for an asset.

    `gh release view` on a draft release returns `url`s under
    `/download/untagged-<hash>/…` which flip to `/download/<tag>/…` once
    the release is published. `latest.json` is generated and validated
    while the release is still a draft, so we construct the eventual
    tagged URL from the tag + asset name instead of trusting the
    draft-state URL.
    """
    return f"{download_base}/{name}"


def build_platforms(
    assets_by_name: dict[str, dict[str, Any]],
    artifacts_dir: Path,
    download_base: str,
) -> dict[str, dict[str, str]]:
    """Build the Tauri updater `platforms` block from release assets + local .sig files."""
    # GitHub normalizes spaces to dots in release-asset names, but
    # actions/download-artifact preserves the original (spaced) filenames.
    # Match local sig files by regex so either naming works. Filter to
    # files: actions/download-artifact creates a directory per artifact
    # named after the file, so an unfiltered glob also yields the parent
    # directory, which read_text() would choke on.
    local_sigs = [p for p in artifacts_dir.rglob("*.sig") if p.is_file()]
    platforms: dict[str, dict[str, str]] = {}
    for plat, regex in PLATFORM_SIG_MATCHERS:
        candidates = [a for a in assets_by_name.values() if regex.match(a["name"])]
        if not candidates:
            _warn(f"No signature asset found for {plat}")
            continue
        sig_asset = candidates[0]
        bin_name = sig_asset["name"][: -len(".sig")]
        bin_asset = assets_by_name.get(bin_name)
        if not bin_asset:
            _warn(f"No matching binary asset for {sig_asset['name']}")
            continue
        local_matches = [p for p in local_sigs if regex.match(p.name)]
        if not local_matches:
            _warn(f"Signature file not found locally: {sig_asset['name']}")
            continue
        platforms[plat] = {
            "signature": local_matches[0].read_text().strip(),
            "url": _asset_url(download_base, bin_asset["name"]),
        }
    return platforms


def build_downloads(
    release_assets: list[dict[str, Any]],
    download_base: str,
) -> dict[str, list[dict[str, Any]]]:
    """Group every distributable installer URL by platform."""
    downloads: dict[str, list[dict[str, Any]]] = {}
    for asset in release_assets:
        name = asset["name"]
        for plat, kind, regex in DOWNLOAD_MATCHERS:
            if regex.search(name):
                downloads.setdefault(plat, []).append({
                    "kind": kind,
                    "url": _asset_url(download_base, name),
                    "size": asset["size"],
                })
                break
    # Stable ordering inside each platform so the manifest doesn't churn
    # between runs purely from asset-listing order.
    for entries in downloads.values():
        entries.sort(key=lambda e: e["kind"])
    return downloads


def build_manifest(
    release: dict[str, Any],
    repo: str,
    tag: str,
    artifacts_dir: Path,
) -> dict[str, Any]:
    """Pure transform: release info + local artifacts → manifest dict.

    Args:
        release: Output of `gh release view --json body,publishedAt,assets`
            (each asset must have at least `name`, `url`, `size`).
        repo: `<owner>/<name>` slug, used to build `release_url`.
        tag: Release tag (e.g. `v0.10.0`), used to build `release_url`
            and to derive the unprefixed `version` field.
        artifacts_dir: Directory containing the per-platform `.sig` files
            uploaded by the build matrix (typically `actions/download-artifact`
            output).
    """
    version = tag.lstrip("v")
    assets = release.get("assets") or []
    assets_by_name = {a["name"]: a for a in assets}
    download_base = f"https://github.com/{repo}/releases/download/{tag}"

    platforms = build_platforms(assets_by_name, artifacts_dir, download_base)
    downloads = build_downloads(assets, download_base)

    pub_date = release.get("publishedAt") or datetime.now(timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ"
    )

    return {
        "$schema": SCHEMA_URL,
        "version": version,
        "notes": _strip_assets_pending_warning(release.get("body") or ""),
        "pub_date": pub_date,
        "release_url": f"https://github.com/{repo}/releases/tag/{tag}",
        "platforms": platforms,
        "downloads": downloads,
    }


def fetch_release(tag: str, repo: str) -> dict[str, Any]:
    """Call `gh` to fetch release info. Requires `gh` to be on PATH and authed."""
    return json.loads(
        subprocess.check_output(
            ["gh", "release", "view", tag, "--repo", repo, "--json", "body,publishedAt,assets"],
            text=True,
        )
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--tag",
        default=os.environ.get("TAG"),
        help="Release tag, e.g. v0.10.0. Defaults to $TAG.",
    )
    parser.add_argument(
        "--repo",
        default=os.environ.get("REPO"),
        help="Repository slug, e.g. esphome/esphome-desktop. Defaults to $REPO.",
    )
    parser.add_argument(
        "--artifacts-dir",
        default="artifacts",
        type=Path,
        help="Directory containing the local .sig files (default: artifacts/).",
    )
    parser.add_argument(
        "--output",
        default="latest.json",
        type=Path,
        help="Output path for the manifest (default: latest.json).",
    )
    parser.add_argument(
        "--release-fixture",
        type=Path,
        help="Load release JSON from a file instead of calling `gh`. "
             "Useful for local testing without network or gh auth.",
    )
    args = parser.parse_args()

    if not args.tag:
        parser.error("--tag (or $TAG) is required")
    if not args.repo:
        parser.error("--repo (or $REPO) is required")

    if args.release_fixture:
        release = json.loads(args.release_fixture.read_text())
    else:
        release = fetch_release(args.tag, args.repo)

    manifest = build_manifest(release, args.repo, args.tag, args.artifacts_dir)

    rendered = json.dumps(manifest, indent=2)
    args.output.write_text(rendered)
    print(rendered)
    return 0


if __name__ == "__main__":
    sys.exit(main())
