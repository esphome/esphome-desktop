#!/usr/bin/env python3
"""Tests for .github/scripts/generate_latest_json.py.

The generator produces latest.json — the manifest that drives in-app
self-updates (tauri-plugin-updater consumes `platforms`) and the download
page (`downloads`). A bug here ships a broken manifest to every user and
silently breaks auto-updates, so the pure transform functions deserve a
regression net. Two real bugs already shipped from this script untouched by
tests:

  * #110 — the generator skipped artifact subdirectories. `actions/
    download-artifact` materialises each `.sig` as ``<name>.sig/<name>.sig``
    (a directory containing the file), so a naive glob yields the parent
    directory, which `read_text()` chokes on. Covered by
    ``test_build_platforms_reads_sig_nested_in_artifact_dir``.
  * #111 — the release-drafter "assets pending" caution block leaked into
    the published notes. Covered by ``test_strip_assets_pending_*``.

pytest suite (maintainer requested pytest, fully typed, no classes). This
adds a `pip install pytest` step to the Scripts Test workflow — it is no
longer pure stdlib — but matches the project's chosen test framework.
"""

from __future__ import annotations

import importlib.util
import json
import re
from pathlib import Path
from tempfile import TemporaryDirectory
from types import ModuleType
from typing import Any

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "generate_latest_json.py"
FIXTURES = REPO_ROOT / "tests" / "fixtures"
RELEASE_FIXTURE = FIXTURES / "release.json"
ARTIFACTS_FIXTURE = FIXTURES / "artifacts"

TAG = "v0.10.0"
REPO = "esphome/esphome-desktop"


def _load_generator() -> ModuleType:
    """Import the generator script by path (it isn't an installable package)."""
    spec = importlib.util.spec_from_file_location("generate_latest_json", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(module)
    return module


gen = _load_generator()


def _release() -> dict[str, Any]:
    return json.loads(RELEASE_FIXTURE.read_text())


def _assets_by_name() -> dict[str, dict[str, Any]]:
    return {a["name"]: a for a in _release()["assets"]}


@pytest.fixture
def manifest() -> dict[str, Any]:
    """End-to-end: fixture release + fixture artifacts -> full manifest."""
    return gen.build_manifest(_release(), REPO, TAG, ARTIFACTS_FIXTURE)


@pytest.fixture
def downloads() -> dict[str, list[dict[str, Any]]]:
    return gen.build_downloads(
        _release()["assets"],
        f"https://github.com/{REPO}/releases/download/{TAG}",
    )


# --- build_manifest: end-to-end -------------------------------------------


def test_version_strips_leading_v(manifest: dict[str, Any]) -> None:
    assert manifest["version"] == "0.10.0"


def test_schema_and_release_url(manifest: dict[str, Any]) -> None:
    assert manifest["$schema"] == gen.SCHEMA_URL
    assert (
        manifest["release_url"]
        == "https://github.com/esphome/esphome-desktop/releases/tag/v0.10.0"
    )


def test_pub_date_from_release(manifest: dict[str, Any]) -> None:
    assert manifest["pub_date"] == "2026-05-22T14:31:08Z"


def test_all_five_updater_platforms_present(manifest: dict[str, Any]) -> None:
    assert set(manifest["platforms"]) == {
        "windows-x86_64",
        "linux-x86_64",
        "linux-aarch64",
        "darwin-aarch64",
        "darwin-x86_64",
    }


def test_platform_url_points_at_binary_not_sig(manifest: dict[str, Any]) -> None:
    url = manifest["platforms"]["linux-x86_64"]["url"]
    assert url.endswith("_amd64.AppImage"), url
    assert not url.endswith(".sig"), url
    # URL is built from the tag + asset name (the eventual published URL),
    # not the draft-state /download/untagged-<hash>/ URL.
    assert "/releases/download/v0.10.0/" in url


def test_platform_signature_is_file_contents(manifest: dict[str, Any]) -> None:
    sig = manifest["platforms"]["windows-x86_64"]["signature"]
    assert sig  # non-empty
    assert "untrusted comment:" in sig
    # No trailing whitespace — the generator strips it.
    assert sig == sig.strip()


# --- #111: assets-pending caution block stripping -------------------------


def test_caution_block_removed_from_notes(manifest: dict[str, Any]) -> None:
    notes = manifest["notes"]
    assert "[!CAUTION]" not in notes
    assert "ASSETS_PENDING" not in notes
    assert "DO NOT PUBLISH" not in notes
    assert notes.startswith("## What's Changed"), repr(notes[:40])


def test_body_without_block_passes_through() -> None:
    body = "## What's Changed\n\n* Something nice.\n"
    assert gen._strip_assets_pending_warning(body) == body


def test_stray_marker_comment_scrubbed() -> None:
    body = "Notes here <!-- ASSETS_PENDING --> still here"
    out = gen._strip_assets_pending_warning(body)
    assert "ASSETS_PENDING" not in out


def test_empty_body() -> None:
    assert gen._strip_assets_pending_warning("") == ""


# --- build_platforms: .sig matching / reading -----------------------------


def test_build_platforms_reads_sig_nested_in_artifact_dir() -> None:
    """Regression for #110.

    `actions/download-artifact` stores each signature as
    ``<name>.sig/<name>.sig`` — a *directory* whose only child is the file.
    The generator must read the inner file and never try to ``read_text()``
    the directory. The shipped fixture already uses this nested layout, so
    building from it must succeed and yield real signatures for every
    platform.
    """
    platforms = gen.build_platforms(
        _assets_by_name(),
        ARTIFACTS_FIXTURE,
        f"https://github.com/{REPO}/releases/download/{TAG}",
    )
    assert len(platforms) == 5
    for plat, entry in platforms.items():
        assert entry["signature"].strip(), f"empty signature for {plat}"


def test_build_platforms_also_handles_flat_sig_layout(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """A flat ``<name>.sig`` file (not nested) must work too."""
    with TemporaryDirectory() as tmp:
        tmpdir = Path(tmp)
        (tmpdir / "ESPHome Device Builder_0.10.0_amd64.AppImage.sig").write_text(
            "flat-sig-contents\n"
        )
        # The other four platforms have no local .sig here and will warn;
        # capsys swallows that noise — this test only asserts the flat layout
        # reads.
        platforms = gen.build_platforms(
            _assets_by_name(),
            tmpdir,
            f"https://github.com/{REPO}/releases/download/{TAG}",
        )
    assert platforms["linux-x86_64"]["signature"] == "flat-sig-contents"


def test_missing_sig_asset_is_warned_and_skipped(
    capsys: pytest.CaptureFixture[str],
) -> None:
    assets = {
        n: a
        for n, a in _assets_by_name().items()
        if "setup.exe" not in n  # drop the windows binary + its .sig
    }
    platforms = gen.build_platforms(
        assets,
        ARTIFACTS_FIXTURE,
        f"https://github.com/{REPO}/releases/download/{TAG}",
    )
    assert "windows-x86_64" not in platforms
    assert "No signature asset found for windows-x86_64" in capsys.readouterr().err


def test_missing_local_sig_file_is_warned_and_skipped(
    capsys: pytest.CaptureFixture[str],
) -> None:
    """Asset listed in the release but no local .sig file downloaded."""
    with TemporaryDirectory() as tmp:
        platforms = gen.build_platforms(
            _assets_by_name(),
            Path(tmp),  # empty — no local sig files at all
            f"https://github.com/{REPO}/releases/download/{TAG}",
        )
    assert platforms == {}
    assert "Signature file not found locally" in capsys.readouterr().err


# --- build_downloads ------------------------------------------------------


def test_kinds_grouped_per_platform(
    downloads: dict[str, list[dict[str, Any]]],
) -> None:
    assert {e["kind"] for e in downloads["linux-x86_64"]} == {
        "appimage",
        "deb",
        "rpm",
    }
    assert {e["kind"] for e in downloads["windows-x86_64"]} == {"nsis"}
    assert {e["kind"] for e in downloads["darwin-aarch64"]} == {"dmg"}


def test_entries_sorted_by_kind(
    downloads: dict[str, list[dict[str, Any]]],
) -> None:
    for entries in downloads.values():
        kinds = [e["kind"] for e in entries]
        assert kinds == sorted(kinds)


def test_app_tar_gz_excluded_from_downloads(
    downloads: dict[str, list[dict[str, Any]]],
) -> None:
    urls = [e["url"] for entries in downloads.values() for e in entries]
    assert not any(u.endswith(".app.tar.gz") for u in urls), urls


def test_sig_files_never_in_downloads(
    downloads: dict[str, list[dict[str, Any]]],
) -> None:
    urls = [e["url"] for entries in downloads.values() for e in entries]
    assert not any(u.endswith(".sig") for u in urls), urls


def test_size_is_carried_through(
    downloads: dict[str, list[dict[str, Any]]],
) -> None:
    # Assert the transform carries the source asset's size through, rather
    # than memorising a literal that breaks silently if the fixture changes.
    nsis_asset = next(
        a for a in _release()["assets"] if a["name"].endswith("-setup.exe")
    )
    assert downloads["windows-x86_64"][0]["size"] == nsis_asset["size"]


# --- pub_date fallback ----------------------------------------------------


def test_missing_published_at_falls_back_to_now() -> None:
    release = _release()
    release.pop("publishedAt", None)
    manifest = gen.build_manifest(release, REPO, TAG, ARTIFACTS_FIXTURE)
    # Falls back to an ISO 8601 UTC timestamp (……Z). We don't assert the
    # exact value, only that it has the published shape.
    assert re.search(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$", manifest["pub_date"])
