#!/usr/bin/env python3
"""Tests for .github/scripts/translations.py.

The Lokalise sync script feeds the release pipeline: ``upload`` pushes
en.json keys to translators and ``download`` fetches the locale files a
release embeds. A bug here could silently ship an English-only release or
prune translated keys, so every path — the pure helpers, the API client
against a real (local) HTTP server, and the CLI dispatch — gets a
regression net. Nothing here touches the real Lokalise API.

pytest suite (maintainer-requested framework, fully typed, no classes).
"""

from __future__ import annotations

import base64
import importlib.util
import io
import json
import os
import subprocess
import sys
import threading
import zipfile
from collections.abc import Iterator
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from types import ModuleType
from typing import Any

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "translations.py"


def _load_module() -> ModuleType:
    spec = importlib.util.spec_from_file_location("translations", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


translations = _load_module()


# --------------------------------------------------------------------------- #
# Pure helpers.
# --------------------------------------------------------------------------- #


def test_to_bcp47_canonicalizes_separators_and_case() -> None:
    assert translations.to_bcp47("zh_CN") == "zh-CN"
    assert translations.to_bcp47("pt-br") == "pt-BR"
    assert translations.to_bcp47("sr_latn_rs") == "sr-Latn-RS"
    assert translations.to_bcp47("es_419") == "es-419"
    assert translations.to_bcp47("EN") == "en"
    # Non-standard subtags pass through lowercased rather than raising.
    assert translations.to_bcp47("x_weird9") == "x-weird9"


def test_locale_from_zip_entry() -> None:
    assert translations.locale_from_zip_entry("fr.json") == "fr"
    assert translations.locale_from_zip_entry("nested/zh_CN.json") == "zh-CN"
    assert translations.locale_from_zip_entry("readme.txt") is None


def _zip_bytes(entries: dict[str, str]) -> bytes:
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as bundle:
        for name, content in entries.items():
            bundle.writestr(name, content)
    return buf.getvalue()


def test_write_locale_bundle_writes_non_base_locales(tmp_path: Path) -> None:
    written = translations.write_locale_bundle(
        _zip_bytes(
            {
                "en.json": '{"a": "english"}',
                "fr.json": '{"a": "français"}',
                "zh_CN.json": '{"a": "中文"}',
                "notes.txt": "not a locale",
            }
        ),
        tmp_path,
    )
    assert written == ["fr", "zh-CN"]
    # en.json is the committed source of truth: never written by a download.
    assert not (tmp_path / "en.json").exists()
    # Repo JSON conventions: 2-space indent, raw unicode, trailing newline.
    assert (tmp_path / "fr.json").read_text(encoding="utf-8") == (
        '{\n  "a": "français"\n}\n'
    )
    assert "中文" in (tmp_path / "zh-CN.json").read_text(encoding="utf-8")


def test_write_locale_bundle_prints_repo_relative_paths(
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
    capsys: pytest.CaptureFixture[str],
) -> None:
    # When the destination sits inside the repo root, the log line shows the
    # repo-relative path (the ValueError fallback is covered by the
    # tmp_path-outside-repo tests above).
    monkeypatch.setattr(translations, "REPO_ROOT", tmp_path)
    dest = tmp_path / "src-tauri" / "translations"
    dest.mkdir(parents=True)
    translations.write_locale_bundle(_zip_bytes({"fr.json": "{}"}), dest)
    assert "src-tauri/translations/fr.json" in capsys.readouterr().out


# --------------------------------------------------------------------------- #
# Fake Lokalise server. Serves scripted responses per (method, path) and
# records request payloads, so the client is exercised over real HTTP.
# --------------------------------------------------------------------------- #


class _FakeLokalise(ThreadingHTTPServer):
    # (method, path) -> list of (status, body bytes); the last entry repeats.
    script: dict[tuple[str, str], list[tuple[int, bytes]]]
    requests: list[tuple[str, str, dict[str, Any] | None]]


class _Handler(BaseHTTPRequestHandler):
    server: _FakeLokalise  # type: ignore[assignment]

    def _serve(self) -> None:
        length = int(self.headers.get("Content-Length") or 0)
        body = json.loads(self.rfile.read(length)) if length else None
        self.server.requests.append((self.command, self.path, body))

        responses = self.server.script.get((self.command, self.path))
        if not responses:
            self.send_response(404)
            self.end_headers()
            return
        status, payload = responses.pop(0) if len(responses) > 1 else responses[0]
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(payload)

    do_GET = _serve
    do_POST = _serve

    def log_message(self, format: str, *args: Any) -> None:  # noqa: A002
        pass  # keep pytest output clean


@pytest.fixture
def fake_lokalise() -> Iterator[_FakeLokalise]:
    server = _FakeLokalise(("127.0.0.1", 0), _Handler)
    server.script = {}
    server.requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    yield server
    server.shutdown()
    thread.join()


def _base_url(server: _FakeLokalise) -> str:
    host, port = server.server_address[0], server.server_address[1]
    return f"http://{host}:{port}"


def _client(server: _FakeLokalise, **kwargs: Any) -> Any:
    return translations.LokaliseClient(
        "token",
        "proj",
        api_base=_base_url(server),
        poll_interval=0.0,
        sleep=lambda _s: None,
        **kwargs,
    )


def _json(payload: dict[str, Any], status: int = 200) -> tuple[int, bytes]:
    return (status, json.dumps(payload).encode())


# --------------------------------------------------------------------------- #
# Client construction and request plumbing.
# --------------------------------------------------------------------------- #


def test_client_requires_token_and_project() -> None:
    with pytest.raises(translations.LokaliseError, match="LOKALISE_API_TOKEN"):
        translations.LokaliseClient("", "proj")
    with pytest.raises(translations.LokaliseError, match="LOKALISE_PROJECT_ID"):
        translations.LokaliseClient("token", "")


def test_request_surfaces_http_errors(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        (401, b'{"error": "bad token"}')
    ]
    with pytest.raises(translations.LokaliseError, match="HTTP 401"):
        _client(fake_lokalise).wait_for_process("p1")


# --------------------------------------------------------------------------- #
# Upload.
# --------------------------------------------------------------------------- #


def test_upload_polls_until_finished(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("POST", "/projects/proj/files/upload")] = [
        _json({"process": {"process_id": "p1"}})
    ]
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        _json({"process": {"status": "queued"}}),
        _json({"process": {"status": "running"}}),
        _json({"process": {"status": "finished", "details": {"files": 1}}}),
    ]

    process = _client(fake_lokalise).upload_base_file("QUJD", cleanup_mode=False)

    assert process["status"] == "finished"
    upload_body = fake_lokalise.requests[0][2]
    assert upload_body is not None
    assert upload_body["data"] == "QUJD"
    assert upload_body["lang_iso"] == "en"
    assert upload_body["cleanup_mode"] is False
    # Placeholder/plural handling must stay verbatim for the Rust runtime.
    assert upload_body["convert_placeholders"] is False
    assert upload_body["detect_icu_plurals"] is False
    assert upload_body["replace_modified"] is True


def test_upload_cleanup_mode_is_forwarded(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("POST", "/projects/proj/files/upload")] = [
        _json({"process": {"process_id": "p1"}})
    ]
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        _json({"process": {"status": "finished"}})
    ]

    _client(fake_lokalise).upload_base_file("QUJD", cleanup_mode=True)

    upload_body = fake_lokalise.requests[0][2]
    assert upload_body is not None
    assert upload_body["cleanup_mode"] is True


def test_upload_without_process_id_fails(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("POST", "/projects/proj/files/upload")] = [_json({})]
    with pytest.raises(translations.LokaliseError, match="process id"):
        _client(fake_lokalise).upload_base_file("QUJD", cleanup_mode=False)


def test_failed_process_raises(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        _json({"process": {"status": "failed"}})
    ]
    with pytest.raises(translations.LokaliseError, match="failed"):
        _client(fake_lokalise).wait_for_process("p1")


def test_stuck_process_times_out(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        _json({"process": {"status": "running"}})
    ]
    client = _client(fake_lokalise, poll_timeout=0.0)
    with pytest.raises(translations.LokaliseError, match="timed out"):
        client.wait_for_process("p1")


# --------------------------------------------------------------------------- #
# Download.
# --------------------------------------------------------------------------- #


def _script_download(fake_lokalise: _FakeLokalise, entries: dict[str, str]) -> None:
    """Script the async-export flow ending in a bundle zip download."""
    bundle_url = f"{_base_url(fake_lokalise)}/bundle.zip"
    fake_lokalise.script[("POST", "/projects/proj/files/async-download")] = [
        _json({"process_id": "p2"})
    ]
    fake_lokalise.script[("GET", "/projects/proj/processes/p2")] = [
        _json(
            {
                "process": {
                    "status": "finished",
                    "details": {"download_url": bundle_url},
                }
            }
        )
    ]
    fake_lokalise.script[("GET", "/bundle.zip")] = [(200, _zip_bytes(entries))]


def test_download_bundle_url_round_trip(fake_lokalise: _FakeLokalise) -> None:
    _script_download(fake_lokalise, {"fr.json": "{}"})

    url = _client(fake_lokalise).download_bundle_url()

    assert url.endswith("/bundle.zip")
    export_body = fake_lokalise.requests[0][2]
    assert export_body is not None
    assert export_body["format"] == "json"
    assert export_body["bundle_structure"] == "%LANG_ISO%.json"
    # Untranslated keys must be skipped so the app's English fallback kicks in.
    assert export_body["export_empty_as"] == "skip"


def test_download_without_process_id_fails(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("POST", "/projects/proj/files/async-download")] = [_json({})]
    with pytest.raises(translations.LokaliseError, match="process id"):
        _client(fake_lokalise).download_bundle_url()


def test_download_without_url_fails(fake_lokalise: _FakeLokalise) -> None:
    fake_lokalise.script[("POST", "/projects/proj/files/async-download")] = [
        _json({"process_id": "p2"})
    ]
    fake_lokalise.script[("GET", "/projects/proj/processes/p2")] = [
        _json({"process": {"status": "finished", "details": {}}})
    ]
    with pytest.raises(translations.LokaliseError, match="download_url"):
        _client(fake_lokalise).download_bundle_url()


def test_run_download_writes_locales(
    fake_lokalise: _FakeLokalise, tmp_path: Path
) -> None:
    _script_download(
        fake_lokalise, {"en.json": "{}", "fr.json": '{"tray": {"quit": "Quitter"}}'}
    )

    assert translations.run_download(_client(fake_lokalise), tmp_path) == 0

    data = json.loads((tmp_path / "fr.json").read_text(encoding="utf-8"))
    assert data == {"tray": {"quit": "Quitter"}}


def test_run_download_fails_on_empty_bundle(
    fake_lokalise: _FakeLokalise, tmp_path: Path
) -> None:
    # Only the base language comes back: a wrong project id or corrupt
    # export must fail loudly, not quietly ship an English-only release.
    _script_download(fake_lokalise, {"en.json": "{}"})
    with pytest.raises(translations.LokaliseError, match="no non-base"):
        translations.run_download(_client(fake_lokalise), tmp_path)


# --------------------------------------------------------------------------- #
# CLI dispatch.
# --------------------------------------------------------------------------- #


def _set_env(monkeypatch: pytest.MonkeyPatch, server: _FakeLokalise) -> None:
    monkeypatch.setenv("LOKALISE_API_TOKEN", "token")
    monkeypatch.setenv("LOKALISE_PROJECT_ID", "proj")
    monkeypatch.setenv("LOKALISE_API_BASE", _base_url(server))


def test_main_upload_reads_repo_en_json(
    fake_lokalise: _FakeLokalise, monkeypatch: pytest.MonkeyPatch
) -> None:
    _set_env(monkeypatch, fake_lokalise)
    fake_lokalise.script[("POST", "/projects/proj/files/upload")] = [
        _json({"process": {"process_id": "p1"}})
    ]
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        _json({"process": {"status": "finished"}})
    ]

    assert translations.main(["upload"]) == 0

    upload_body = fake_lokalise.requests[0][2]
    assert upload_body is not None
    # The upload carries the real committed en.json.
    en_json = (REPO_ROOT / "src-tauri" / "translations" / "en.json").read_bytes()
    assert upload_body["data"] == base64.b64encode(en_json).decode()
    assert upload_body["cleanup_mode"] is False


def test_main_upload_cleanup_flag(
    fake_lokalise: _FakeLokalise, monkeypatch: pytest.MonkeyPatch
) -> None:
    _set_env(monkeypatch, fake_lokalise)
    fake_lokalise.script[("POST", "/projects/proj/files/upload")] = [
        _json({"process": {"process_id": "p1"}})
    ]
    fake_lokalise.script[("GET", "/projects/proj/processes/p1")] = [
        _json({"process": {"status": "finished"}})
    ]

    assert translations.main(["upload", "--cleanup"]) == 0

    upload_body = fake_lokalise.requests[0][2]
    assert upload_body is not None
    assert upload_body["cleanup_mode"] is True


def test_main_download_writes_into_translations_dir(
    fake_lokalise: _FakeLokalise,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    _set_env(monkeypatch, fake_lokalise)
    monkeypatch.setattr(translations, "TRANSLATIONS_DIR", tmp_path)
    _script_download(fake_lokalise, {"fr.json": '{"a": "b"}'})

    assert translations.main(["download"]) == 0
    assert (tmp_path / "fr.json").exists()


def test_main_reports_errors_and_exits_nonzero(
    monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    monkeypatch.delenv("LOKALISE_API_TOKEN", raising=False)
    monkeypatch.delenv("LOKALISE_PROJECT_ID", raising=False)

    assert translations.main(["download"]) == 1
    assert "::error::" in capsys.readouterr().err


def test_script_entry_point_runs_main() -> None:
    # Executed as a real subprocess to cover the `__main__` guard; with no
    # credentials in the environment it must exit 1 with a CI error line.
    env = {k: v for k, v in os.environ.items() if not k.startswith("LOKALISE_")}
    result = subprocess.run(
        [sys.executable, str(SCRIPT_PATH), "download"],
        capture_output=True,
        text=True,
        env=env,
        check=False,
    )
    assert result.returncode == 1
    assert "::error::" in result.stderr
