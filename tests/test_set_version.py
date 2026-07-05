#!/usr/bin/env python3
"""Tests for .github/scripts/set_version.py.

The release-drafter workflow rewrites the release version in every
version-bearing file through this script's file-to-pattern table. A bug here
ships a mismatched version and breaks download URLs built from it, so the
rewrites get a regression net: each transform must produce exactly the line
the old per-file sed steps produced, and a non-matching pattern must fail
loudly rather than silently skip a file.

pytest suite (maintainer-requested framework, fully typed, no classes).
"""

from __future__ import annotations

import importlib.util
import re
import sys
from pathlib import Path
from types import ModuleType

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "set_version.py"

# Trimmed copies of the version-bearing files, in the same shape as the real
# ones (the details that keep the patterns honest: a dependency `version =` in
# Cargo.toml, indentation and the trailing comma in tauri.conf.json, and a
# `$pkgver` interpolation in the PKGBUILD).
CARGO_TOML = """\
[package]
name = "esphome-desktop"
version = "0.14.2"
edition = "2021"

[dependencies]
tauri = { version = "2.11.2", features = ["tray-icon"] }
"""

TAURI_CONF = """\
{
  "$schema": "https://schema.tauri.app/config/2",
  "productName": "ESPHome Device Builder",
  "version": "0.14.2",
  "identifier": "com.esphome.desktop"
}
"""

PKGBUILD = """\
pkgname=esphome-desktop-bin
pkgver=0.14.2
pkgrel=1
source=("$pkgname-$pkgver.deb::$url/releases/download/v${pkgver}/app_${pkgver}_amd64.deb")
"""


def _load_module() -> ModuleType:
    spec = importlib.util.spec_from_file_location("set_version", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


set_version = _load_module()


def _table_entry(rel_path: str) -> tuple[re.Pattern[str], str]:
    """Return `(pattern, template)` for a file in the VERSION_FILES table."""
    for path, pattern, template in set_version.VERSION_FILES:
        if path == rel_path:
            return pattern, template
    raise KeyError(rel_path)


# --------------------------------------------------------------------------- #
# Per-file transforms: byte-identical to the old sed steps.
# --------------------------------------------------------------------------- #


def test_cargo_toml_rewrites_package_version_only() -> None:
    pattern, template = _table_entry("src-tauri/Cargo.toml")
    out = set_version.set_version(CARGO_TOML, pattern, template, "0.15.0")
    # Same edit the old `sed "s/^version = .*/version = \"0.15.0\"/g"` made.
    assert out == CARGO_TOML.replace('version = "0.14.2"', 'version = "0.15.0"')
    # The anchored pattern must not touch dependency `version =` fields.
    assert 'tauri = { version = "2.11.2", features = ["tray-icon"] }' in out


def test_tauri_conf_keeps_indent_and_trailing_comma() -> None:
    pattern, template = _table_entry("src-tauri/tauri.conf.json")
    out = set_version.set_version(TAURI_CONF, pattern, template, "0.15.0")
    # Same edit the old `sed "s/\"version\": .*/\"version\": \"0.15.0\",/g"`
    # made: the leading indent survives and the trailing comma is emitted.
    assert out == TAURI_CONF.replace('"version": "0.14.2",', '"version": "0.15.0",')
    assert '  "version": "0.15.0",\n' in out


def test_pkgbuild_rewrites_pkgver_only() -> None:
    pattern, template = _table_entry("packaging/aur/esphome-desktop-bin/PKGBUILD")
    out = set_version.set_version(PKGBUILD, pattern, template, "0.15.0")
    # Same edit the old `sed "s/^pkgver=.*/pkgver=0.15.0/g"` made.
    assert out == PKGBUILD.replace("pkgver=0.14.2", "pkgver=0.15.0")
    # The $pkgver interpolations in source= stay as interpolations.
    assert "v${pkgver}/app_${pkgver}_amd64.deb" in out


def test_set_version_raises_when_pattern_matches_nothing() -> None:
    pattern, template = _table_entry("src-tauri/Cargo.toml")
    with pytest.raises(ValueError, match="matched 0 lines"):
        set_version.set_version('[package]\nname = "x"\n', pattern, template, "1.0.0")


def test_set_version_raises_when_pattern_matches_more_than_once() -> None:
    # An over-broad pattern must fail loudly instead of rewriting every match.
    pattern, template = _table_entry("src-tauri/Cargo.toml")
    text = 'version = "0.1.0"\nversion = "0.2.0"\n'
    with pytest.raises(ValueError, match="matched 2 lines"):
        set_version.set_version(text, pattern, template, "1.0.0")


def test_table_covers_the_real_files() -> None:
    # Every table entry must point at an existing file whose pattern matches,
    # so a rename or restructure fails here before it fails a release.
    for rel_path, pattern, _template in set_version.VERSION_FILES:
        text = (REPO_ROOT / rel_path).read_text(encoding="utf-8")
        assert pattern.search(text), f"{rel_path}: pattern matched nothing"


# --------------------------------------------------------------------------- #
# CLI behaviour.
# --------------------------------------------------------------------------- #


def _write_tree(root: Path) -> None:
    (root / "src-tauri").mkdir()
    (root / "packaging" / "aur" / "esphome-desktop-bin").mkdir(parents=True)
    (root / "src-tauri" / "Cargo.toml").write_text(CARGO_TOML, encoding="utf-8")
    (root / "src-tauri" / "tauri.conf.json").write_text(TAURI_CONF, encoding="utf-8")
    (root / "packaging" / "aur" / "esphome-desktop-bin" / "PKGBUILD").write_text(
        PKGBUILD, encoding="utf-8"
    )


def test_main_rewrites_every_file(tmp_path: Path) -> None:
    _write_tree(tmp_path)
    rc = set_version.main(["0.15.0", "--root", str(tmp_path)])
    assert rc == 0
    assert 'version = "0.15.0"' in (tmp_path / "src-tauri" / "Cargo.toml").read_text(
        encoding="utf-8"
    )
    assert '"version": "0.15.0",' in (
        tmp_path / "src-tauri" / "tauri.conf.json"
    ).read_text(encoding="utf-8")
    assert "pkgver=0.15.0" in (
        tmp_path / "packaging" / "aur" / "esphome-desktop-bin" / "PKGBUILD"
    ).read_text(encoding="utf-8")


def test_main_fails_when_a_pattern_matches_nothing(tmp_path: Path) -> None:
    # A restructured file must fail the release job, not silently keep its old
    # version while the other files move on.
    _write_tree(tmp_path)
    (tmp_path / "src-tauri" / "tauri.conf.json").write_text("{}\n", encoding="utf-8")
    rc = set_version.main(["0.15.0", "--root", str(tmp_path)])
    assert rc == 1
    # Writes happen only after every pattern matched, so the earlier files in
    # the table must be untouched, not left partially bumped.
    assert (tmp_path / "src-tauri" / "Cargo.toml").read_text(
        encoding="utf-8"
    ) == CARGO_TOML
    assert (
        tmp_path / "packaging" / "aur" / "esphome-desktop-bin" / "PKGBUILD"
    ).read_text(encoding="utf-8") == PKGBUILD


def test_main_fails_with_error_message_when_a_file_is_missing(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    # A missing file must produce the ::error:: annotation naming the file and
    # a non-zero exit, not a raw traceback, and must not touch the other files.
    _write_tree(tmp_path)
    (tmp_path / "src-tauri" / "tauri.conf.json").unlink()
    rc = set_version.main(["0.15.0", "--root", str(tmp_path)])
    assert rc == 1
    assert "::error::src-tauri/tauri.conf.json:" in capsys.readouterr().err
    assert (tmp_path / "src-tauri" / "Cargo.toml").read_text(
        encoding="utf-8"
    ) == CARGO_TOML
