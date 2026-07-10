#!/usr/bin/env python3
"""Sync UI translations with Lokalise.

    python3 .github/scripts/translations.py upload [--cleanup]
    python3 .github/scripts/translations.py download

The base language file (src-tauri/translations/en.json) is the in-repo
source of truth: ``upload`` pushes its keys to Lokalise, adding new keys and
updating the English copy of existing keys (other locales are untouched);
``download`` writes every other locale back into src-tauri/translations/ and
never touches en.json. Non-English locales are gitignored — they only exist
in a checkout when this script downloads them (the release build embeds
whatever is present, see src-tauri/build.rs).

Automated uploads never prune: keys removed from en.json are left in the
Lokalise project so a moved key keeps its translations for reuse. To delete
stale keys, run ``upload --cleanup`` (exposed as a manual input on the
upload workflow).

Credentials come from the environment:

    LOKALISE_API_TOKEN   API token with read/write file permissions
    LOKALISE_PROJECT_ID  target project id
    LOKALISE_API_BASE    optional API base URL override (used by the tests)

The script fails loudly (non-zero exit) when credentials are missing, the
API errors, or a download yields no translated locales — a silent no-op
would let a release quietly ship English-only. Mirrors
device-builder-frontend's build-scripts/translations.ts.
"""

from __future__ import annotations

import argparse
import base64
import io
import json
import os
import sys
import time
import urllib.error
import urllib.request
import zipfile
from collections.abc import Callable
from pathlib import Path
from typing import Any

API_BASE = "https://api.lokalise.com/api2"

# Base language. Its file (en.json) is the only committed translation file
# and is never overwritten by a download.
BASE_LANGUAGE = "en"

HTTP_TIMEOUT = 30.0

# File upload and export are both asynchronous on Lokalise's side: the
# endpoint returns a process id and the work happens in the background.
# Poll the process until it leaves the queued/running state, with a ceiling
# so a stuck process can't hang CI.
POLL_INTERVAL = 2.0
POLL_TIMEOUT = 300.0

REPO_ROOT = Path(__file__).resolve().parents[2]
TRANSLATIONS_DIR = REPO_ROOT / "src-tauri" / "translations"


class LokaliseError(RuntimeError):
    """A Lokalise API call failed or returned an unusable payload."""


def to_bcp47(stem: str) -> str:
    """Canonicalize a locale stem to a BCP 47 tag.

    Lokalise emits underscore-separated ISO codes (``zh_CN``, ``pt_BR``);
    normalize the separator and per-subtag case so the written filename is a
    valid tag (``zh-CN``, ``pt-BR``, ``sr-Latn-RS``) regardless of what the
    export used.
    """
    parts = stem.replace("_", "-").split("-")
    out: list[str] = [parts[0].lower()]
    for part in parts[1:]:
        if len(part) == 4 and part.isalpha():
            out.append(part.title())  # script subtag, e.g. Latn
        elif (len(part) == 2 and part.isalpha()) or (len(part) == 3 and part.isdigit()):
            out.append(part.upper())  # region subtag, e.g. CN / 419
        else:
            out.append(part.lower())
    return "-".join(out)


def locale_from_zip_entry(name: str) -> str | None:
    """Canonical locale code for a zip entry (``fr.json``, ``x/zh_CN.json``),
    or None when the entry isn't a JSON file."""
    if not name.endswith(".json"):
        return None
    stem = name.rsplit("/", 1)[-1].removesuffix(".json")
    return to_bcp47(stem)


class LokaliseClient:
    """Minimal Lokalise REST API v2 client (stdlib urllib only)."""

    def __init__(
        self,
        token: str,
        project_id: str,
        api_base: str = API_BASE,
        poll_interval: float = POLL_INTERVAL,
        poll_timeout: float = POLL_TIMEOUT,
        sleep: Callable[[float], None] = time.sleep,
    ) -> None:
        if not token:
            raise LokaliseError(
                "Lokalise API token is required (set LOKALISE_API_TOKEN)."
            )
        if not project_id:
            raise LokaliseError(
                "Lokalise project id is required (set LOKALISE_PROJECT_ID)."
            )
        self.token = token
        self.project_id = project_id
        self.api_base = api_base
        self.poll_interval = poll_interval
        self.poll_timeout = poll_timeout
        self.sleep = sleep

    def _request(
        self, method: str, path: str, body: dict[str, Any] | None = None
    ) -> dict[str, Any]:
        url = f"{self.api_base}/projects/{self.project_id}/{path}"
        headers = {"X-Api-Token": self.token, "Accept": "application/json"}
        data: bytes | None = None
        if body is not None:
            headers["Content-Type"] = "application/json"
            data = json.dumps(body).encode()
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:  # noqa: S310
                payload: dict[str, Any] = json.load(resp)
                return payload
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode(errors="replace")
            raise LokaliseError(
                f"Lokalise {method} {path} failed: HTTP {exc.code} {detail}"
            ) from exc
        except (urllib.error.URLError, TimeoutError) as exc:
            # Network-level failures (DNS, refused connection, timeout) must
            # surface as a single ::error:: CI line, not a stack trace.
            raise LokaliseError(f"Lokalise {method} {path} failed: {exc}") from exc

    def upload_base_file(self, data_b64: str, *, cleanup_mode: bool) -> dict[str, Any]:
        """Upload the base-language file and wait for processing to finish."""
        payload = {
            "data": data_b64,
            "filename": f"{BASE_LANGUAGE}.json",
            "lang_iso": BASE_LANGUAGE,
            # The strings use `{placeholder}` tokens the Rust runtime fills
            # verbatim; don't let Lokalise rewrite them into its universal
            # placeholder format.
            "convert_placeholders": False,
            # The Rust runtime does plain token replacement, not ICU — don't
            # let Lokalise split plural-looking strings into per-form keys
            # that would re-export as ICU syntax the app can't render.
            "detect_icu_plurals": False,
            # Push reworded English copy for existing keys, not just new
            # keys — en.json is the source of truth for the base language.
            # Only the English file is uploaded, so this never clobbers
            # translator edits in other locales.
            "replace_modified": True,
            # When set, keys absent from the uploaded base file are deleted
            # from the project. Off by default; opt in via `upload --cleanup`.
            "cleanup_mode": cleanup_mode,
        }
        result = self._request("POST", "files/upload", payload)
        process_id = (result.get("process") or {}).get("process_id")
        if not process_id:
            raise LokaliseError(f"Upload did not return a process id: {result}")
        return self.wait_for_process(process_id)

    def wait_for_process(self, process_id: str) -> dict[str, Any]:
        """Poll a queued process (upload or async export) until it finishes."""
        deadline = time.monotonic() + self.poll_timeout
        while True:
            result = self._request("GET", f"processes/{process_id}")
            process: dict[str, Any] = result.get("process") or {}
            status = process.get("status")
            if status == "finished":
                return process
            if status in ("failed", "cancelled"):
                raise LokaliseError(
                    f"Lokalise process {process_id} {status}: {process}"
                )
            if time.monotonic() > deadline:
                raise LokaliseError(
                    f"Lokalise process {process_id} timed out (last status: {status})."
                )
            self.sleep(self.poll_interval)

    def download_bundle_url(self) -> str:
        """Request an export bundle for every project language and return its
        download URL.

        Uses the async export endpoint (files/async-download): it returns a
        process id, and the bundle URL lands in the finished process's
        ``details.download_url`` — same poller as upload.
        """
        payload = {
            "format": "json",
            "original_filenames": False,
            "bundle_structure": "%LANG_ISO%.json",
            # Omit untranslated keys so the app's per-key English fallback
            # kicks in (src-tauri/src/i18n) instead of shipping English
            # strings inside non-English files.
            "export_empty_as": "skip",
            "export_sort": "first_added",
            "json_unescaped_slashes": True,
            "replace_breaks": False,
            "indentation": "2sp",
            # Keep `{name}` token style on export to match the runtime.
            "placeholder_format": "icu",
            # No filter_langs: export whatever languages the project has, so
            # adding a locale in Lokalise round-trips with no code change.
        }
        result = self._request("POST", "files/async-download", payload)
        process_id = result.get("process_id")
        if not process_id:
            raise LokaliseError(f"Async download did not return a process id: {result}")
        process = self.wait_for_process(process_id)
        bundle_url = (process.get("details") or {}).get("download_url")
        if not bundle_url:
            raise LokaliseError(
                f"Async download process {process_id} finished without a "
                f"download_url: {process}"
            )
        return str(bundle_url)


def fetch_bytes(url: str) -> bytes:
    """GET a URL (the signed bundle blob) and return the raw bytes."""
    req = urllib.request.Request(url, headers={"Accept": "application/octet-stream"})
    try:
        with urllib.request.urlopen(req, timeout=HTTP_TIMEOUT) as resp:  # noqa: S310
            return resp.read()
    except (urllib.error.URLError, TimeoutError) as exc:
        # HTTPError is a URLError subclass, so an expired/403 signed URL lands
        # here too — surface one ::error:: CI line instead of a stack trace.
        raise LokaliseError(f"Failed to download bundle: {exc}") from exc


def write_locale_bundle(zip_bytes: bytes, dest_dir: Path) -> list[str]:
    """Unpack a zip of ``<locale>.json`` files into ``dest_dir``.

    Writes each locale except the base — en.json is the in-repo source of
    truth and is never overwritten by a download. Stems are canonicalized to
    BCP 47 so a Lokalise ``zh_CN.json`` lands as ``zh-CN.json``. Returns the
    sorted locales written.
    """
    written: list[str] = []
    try:
        bundle = zipfile.ZipFile(io.BytesIO(zip_bytes))
    except zipfile.BadZipFile as exc:
        raise LokaliseError(f"Bundle is not a valid zip archive: {exc}") from exc
    with bundle:
        for info in bundle.infolist():
            locale = locale_from_zip_entry(info.filename)
            if locale is None or locale == BASE_LANGUAGE:
                continue
            try:
                data = json.loads(bundle.read(info))
            except json.JSONDecodeError as exc:
                # A corrupt/partial export must fail as a single ::error:: CI
                # line, not a stack trace.
                raise LokaliseError(
                    f"Bundle entry '{info.filename}' is not valid JSON: {exc}"
                ) from exc
            path = dest_dir / f"{locale}.json"
            # Re-serialize with the repo's JSON conventions (2-space indent,
            # raw unicode, trailing newline) so a diff only carries genuine
            # translation changes.
            path.write_text(
                json.dumps(data, ensure_ascii=False, indent=2) + "\n",
                encoding="utf-8",
            )
            try:
                display = path.relative_to(REPO_ROOT)
            except ValueError:  # dest outside the repo (tests)
                display = path
            print(f"  {display}")
            written.append(locale)
    return sorted(written)


def run_upload(client: LokaliseClient, *, cleanup: bool) -> int:
    data_b64 = base64.b64encode(
        (TRANSLATIONS_DIR / f"{BASE_LANGUAGE}.json").read_bytes()
    ).decode()

    suffix = " (cleanup: removing keys absent from en.json)" if cleanup else ""
    print(f"Uploading {BASE_LANGUAGE}.json as base language '{BASE_LANGUAGE}'{suffix}")

    process = client.upload_base_file(data_b64, cleanup_mode=cleanup)
    print(f"Upload finished (status: {process.get('status', 'unknown')}).")
    return 0


def run_download(client: LokaliseClient, dest_dir: Path) -> int:
    print("Requesting bundle for all project languages from Lokalise")
    bundle_url = client.download_bundle_url()
    written = write_locale_bundle(fetch_bytes(bundle_url), dest_dir)

    if not written:
        # A download that yields no locales is a real failure (wrong project
        # id, empty/corrupt bundle, API hiccup) — fail loudly so a release
        # can't silently ship English-only. The legitimate English-only case
        # is the unset-secrets guard in the workflow, which never gets here.
        raise LokaliseError("Lokalise returned no non-base translation files.")
    print(f"Wrote {len(written)} file(s): {', '.join(written)}")
    return 0


def client_from_env() -> LokaliseClient:
    return LokaliseClient(
        os.environ.get("LOKALISE_API_TOKEN", ""),
        os.environ.get("LOKALISE_PROJECT_ID", ""),
        api_base=os.environ.get("LOKALISE_API_BASE", API_BASE),
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    upload = sub.add_parser("upload", help="Push en.json keys to Lokalise")
    upload.add_argument(
        "--cleanup",
        action="store_true",
        help="Delete Lokalise keys no longer in en.json (prunes stale keys)",
    )
    sub.add_parser("download", help="Pull translated locales from Lokalise")
    args = parser.parse_args(argv)

    try:
        client = client_from_env()
        if args.command == "upload":
            return run_upload(client, cleanup=args.cleanup)
        return run_download(client, TRANSLATIONS_DIR)
    except LokaliseError as exc:
        print(f"::error::{exc}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
