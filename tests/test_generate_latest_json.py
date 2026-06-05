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

Pure stdlib (``unittest``) so it runs with ``python3 -m unittest`` and needs
no pip install — mirrors the dependency-light philosophy of the rest of CI.
"""

from __future__ import annotations

import importlib.util
import io
import json
import unittest
from contextlib import redirect_stderr
from pathlib import Path
from tempfile import TemporaryDirectory

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "generate_latest_json.py"
FIXTURES = REPO_ROOT / "tests" / "fixtures"
RELEASE_FIXTURE = FIXTURES / "release.json"
ARTIFACTS_FIXTURE = FIXTURES / "artifacts"

TAG = "v0.10.0"
REPO = "esphome/esphome-desktop"


def _load_generator():
    """Import the generator script by path (it isn't an installable package)."""
    spec = importlib.util.spec_from_file_location("generate_latest_json", SCRIPT_PATH)
    module = importlib.util.module_from_spec(spec)
    assert spec and spec.loader
    spec.loader.exec_module(module)
    return module


gen = _load_generator()


def _release() -> dict:
    return json.loads(RELEASE_FIXTURE.read_text())


class BuildManifestTest(unittest.TestCase):
    """End-to-end: fixture release + fixture artifacts -> full manifest."""

    def setUp(self) -> None:
        self.manifest = gen.build_manifest(_release(), REPO, TAG, ARTIFACTS_FIXTURE)

    def test_version_strips_leading_v(self) -> None:
        self.assertEqual(self.manifest["version"], "0.10.0")

    def test_schema_and_release_url(self) -> None:
        self.assertEqual(self.manifest["$schema"], gen.SCHEMA_URL)
        self.assertEqual(
            self.manifest["release_url"],
            "https://github.com/esphome/esphome-desktop/releases/tag/v0.10.0",
        )

    def test_pub_date_from_release(self) -> None:
        self.assertEqual(self.manifest["pub_date"], "2026-05-22T14:31:08Z")

    def test_all_five_updater_platforms_present(self) -> None:
        self.assertEqual(
            set(self.manifest["platforms"]),
            {
                "windows-x86_64",
                "linux-x86_64",
                "linux-aarch64",
                "darwin-aarch64",
                "darwin-x86_64",
            },
        )

    def test_platform_url_points_at_binary_not_sig(self) -> None:
        url = self.manifest["platforms"]["linux-x86_64"]["url"]
        self.assertTrue(url.endswith("_amd64.AppImage"), url)
        self.assertFalse(url.endswith(".sig"), url)
        # URL is built from the tag + asset name (the eventual published URL),
        # not the draft-state /download/untagged-<hash>/ URL.
        self.assertIn("/releases/download/v0.10.0/", url)

    def test_platform_signature_is_file_contents(self) -> None:
        sig = self.manifest["platforms"]["windows-x86_64"]["signature"]
        self.assertTrue(sig)  # non-empty
        self.assertIn("untrusted comment:", sig)
        # No trailing whitespace — the generator strips it.
        self.assertEqual(sig, sig.strip())


class StripAssetsPendingTest(unittest.TestCase):
    """Regression for #111 — release-drafter caution block in published notes."""

    def test_caution_block_removed_from_notes(self) -> None:
        manifest = gen.build_manifest(_release(), REPO, TAG, ARTIFACTS_FIXTURE)
        notes = manifest["notes"]
        self.assertNotIn("[!CAUTION]", notes)
        self.assertNotIn("ASSETS_PENDING", notes)
        self.assertNotIn("DO NOT PUBLISH", notes)
        self.assertTrue(notes.startswith("## What's Changed"), repr(notes[:40]))

    def test_body_without_block_passes_through(self) -> None:
        body = "## What's Changed\n\n* Something nice.\n"
        self.assertEqual(gen._strip_assets_pending_warning(body), body)

    def test_stray_marker_comment_scrubbed(self) -> None:
        body = "Notes here <!-- ASSETS_PENDING --> still here"
        out = gen._strip_assets_pending_warning(body)
        self.assertNotIn("ASSETS_PENDING", out)

    def test_empty_body(self) -> None:
        self.assertEqual(gen._strip_assets_pending_warning(""), "")


class BuildPlatformsTest(unittest.TestCase):
    """Targeted tests for the .sig matching / reading logic."""

    def _assets_by_name(self) -> dict:
        return {a["name"]: a for a in _release()["assets"]}

    def test_build_platforms_reads_sig_nested_in_artifact_dir(self) -> None:
        """Regression for #110.

        `actions/download-artifact` stores each signature as
        ``<name>.sig/<name>.sig`` — a *directory* whose only child is the
        file. The generator must read the inner file and never try to
        ``read_text()`` the directory. The shipped fixture already uses this
        nested layout, so building from it must succeed and yield real
        signatures for every platform.
        """
        platforms = gen.build_platforms(
            self._assets_by_name(),
            ARTIFACTS_FIXTURE,
            f"https://github.com/{REPO}/releases/download/{TAG}",
        )
        self.assertEqual(len(platforms), 5)
        for plat, entry in platforms.items():
            self.assertTrue(entry["signature"].strip(), f"empty signature for {plat}")

    def test_build_platforms_also_handles_flat_sig_layout(self) -> None:
        """A flat ``<name>.sig`` file (not nested) must work too."""
        with TemporaryDirectory() as tmp:
            tmpdir = Path(tmp)
            (tmpdir / "ESPHome Device Builder_0.10.0_amd64.AppImage.sig").write_text(
                "flat-sig-contents\n"
            )
            # The other four platforms have no local .sig here and will warn;
            # swallow that noise — this test only asserts the flat layout reads.
            with redirect_stderr(io.StringIO()):
                platforms = gen.build_platforms(
                    self._assets_by_name(),
                    tmpdir,
                    f"https://github.com/{REPO}/releases/download/{TAG}",
                )
        self.assertEqual(platforms["linux-x86_64"]["signature"], "flat-sig-contents")

    def test_missing_sig_asset_is_warned_and_skipped(self) -> None:
        assets = {
            n: a
            for n, a in self._assets_by_name().items()
            if "setup.exe" not in n  # drop the windows binary + its .sig
        }
        stderr = io.StringIO()
        with redirect_stderr(stderr):
            platforms = gen.build_platforms(
                assets,
                ARTIFACTS_FIXTURE,
                f"https://github.com/{REPO}/releases/download/{TAG}",
            )
        self.assertNotIn("windows-x86_64", platforms)
        self.assertIn("No signature asset found for windows-x86_64", stderr.getvalue())

    def test_missing_local_sig_file_is_warned_and_skipped(self) -> None:
        """Asset listed in the release but no local .sig file downloaded."""
        with TemporaryDirectory() as tmp:
            stderr = io.StringIO()
            with redirect_stderr(stderr):
                platforms = gen.build_platforms(
                    self._assets_by_name(),
                    Path(tmp),  # empty — no local sig files at all
                    f"https://github.com/{REPO}/releases/download/{TAG}",
                )
        self.assertEqual(platforms, {})
        self.assertIn("Signature file not found locally", stderr.getvalue())


class BuildDownloadsTest(unittest.TestCase):
    def setUp(self) -> None:
        self.downloads = gen.build_downloads(
            _release()["assets"],
            f"https://github.com/{REPO}/releases/download/{TAG}",
        )

    def test_kinds_grouped_per_platform(self) -> None:
        self.assertEqual(
            {e["kind"] for e in self.downloads["linux-x86_64"]},
            {"appimage", "deb", "rpm"},
        )
        self.assertEqual(
            {e["kind"] for e in self.downloads["windows-x86_64"]},
            {"nsis"},
        )
        self.assertEqual(
            {e["kind"] for e in self.downloads["darwin-aarch64"]},
            {"dmg"},
        )

    def test_entries_sorted_by_kind(self) -> None:
        for entries in self.downloads.values():
            kinds = [e["kind"] for e in entries]
            self.assertEqual(kinds, sorted(kinds))

    def test_app_tar_gz_excluded_from_downloads(self) -> None:
        urls = [e["url"] for entries in self.downloads.values() for e in entries]
        self.assertFalse(any(u.endswith(".app.tar.gz") for u in urls), urls)

    def test_sig_files_never_in_downloads(self) -> None:
        urls = [e["url"] for entries in self.downloads.values() for e in entries]
        self.assertFalse(any(u.endswith(".sig") for u in urls), urls)

    def test_size_is_carried_through(self) -> None:
        nsis = self.downloads["windows-x86_64"][0]
        self.assertEqual(nsis["size"], 175342211)


class PubDateFallbackTest(unittest.TestCase):
    def test_missing_published_at_falls_back_to_now(self) -> None:
        release = _release()
        release.pop("publishedAt", None)
        manifest = gen.build_manifest(release, REPO, TAG, ARTIFACTS_FIXTURE)
        # Falls back to an ISO 8601 UTC timestamp (……Z). We don't assert the
        # exact value, only that it has the published shape.
        self.assertRegex(
            manifest["pub_date"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$"
        )


if __name__ == "__main__":
    unittest.main()
