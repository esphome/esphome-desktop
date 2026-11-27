#!/usr/bin/env python3
"""Tests for .github/scripts/verify_updater_signatures.py.

The real-format case uses a throwaway key pair and a signature made by
`cargo tauri signer sign`, so the parser is checked against what Tauri
actually writes; the private key was discarded. The other cases build
minisign files with a test key so each failure path can be hit directly.

pytest suite, fully typed, no classes.
"""

from __future__ import annotations

import base64
import hashlib
import json
import runpy
import sys
from collections.abc import Callable
from pathlib import Path

import pytest
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat
from script_loader import load_script_module

REPO_ROOT = Path(__file__).resolve().parent.parent
SCRIPT_PATH = REPO_ROOT / ".github" / "scripts" / "verify_updater_signatures.py"
SCRIPT = load_script_module(SCRIPT_PATH)

# Made with `cargo tauri signer generate` and `cargo tauri signer sign`.
TAURI_PUBKEY = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEE0QTIyM0I5RjRCNkNFQ0EK"
    "UldUS3pyYjB1U09pcEFlRnZWekRESjhjbDlRa0pzZ3poUlRpUndkK0s3WTJzSGNmRkNpTUExbm0K"
)
TAURI_PAYLOAD = b"hello payload\n"
TAURI_SIGNATURE = (
    "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUS3py"
    "YjB1U09pcENoYVBxOWpxdEVyREt2Y0h0Z2RLM2psTldqcVVsR0lnS1h4K3FDd01nQ01md1VkOTFt"
    "bUJkdmhkaXluY3c4SHhRb1NUbnRLdk9hV0NmNVE1N3FPK1FrPQp0cnVzdGVkIGNvbW1lbnQ6IHRp"
    "bWVzdGFtcDoxNzk1NzY5MTU1CWZpbGU6cGF5bG9hZC5iaW4KTzczclV4SDluZEVYclA3ekdwcDA3"
    "aWwrSlJ5cHkzb2d4MTFBVC9JcEJ0eVRRSkhFSTcvR240RFBoV3lxZWpEMmF3RzBuNkpYcVJQa1Vs"
    "TC9IZWVsQ0E9PQo="
)

KEY_ID = bytes.fromhex("0102030405060708")


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode()


def _pubkey(private: Ed25519PrivateKey, key_id: bytes = KEY_ID) -> str:
    raw = private.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    text = f"untrusted comment: minisign public key\n{_b64(b'Ed' + key_id + raw)}\n"
    return _b64(text.encode())


def _signature(
    private: Ed25519PrivateKey,
    payload: bytes,
    algorithm: bytes = b"ED",
    key_id: bytes = KEY_ID,
    trusted: str = "timestamp:1\tfile:payload",
) -> str:
    message = (
        hashlib.blake2b(payload, digest_size=64).digest()
        if algorithm == b"ED"
        else payload
    )
    signature = private.sign(message)
    global_signature = private.sign(signature + trusted.encode())
    text = (
        "untrusted comment: signature from tauri secret key\n"
        f"{_b64(algorithm + key_id + signature)}\n"
        f"trusted comment: {trusted}\n"
        f"{_b64(global_signature)}\n"
    )
    return _b64(text.encode())


def _write(
    tmp_path: Path, payload: bytes, signature: str, name: str = "app.deb"
) -> Path:
    root = tmp_path / "artifacts"
    (root / name).mkdir(parents=True)
    (root / name / name).write_bytes(payload)
    (root / f"{name}.sig").mkdir()
    (root / f"{name}.sig" / f"{name}.sig").write_text(signature, encoding="utf-8")
    return root


def _config(tmp_path: Path, pubkey: str) -> Path:
    path = tmp_path / "tauri.conf.json"
    path.write_text(json.dumps({"plugins": {"updater": {"pubkey": pubkey}}}))
    return path


def test_real_tauri_signature_verifies(tmp_path: Path) -> None:
    root = _write(tmp_path, TAURI_PAYLOAD, TAURI_SIGNATURE, "payload.bin")
    args = ["--config", str(_config(tmp_path, TAURI_PUBKEY)), str(root)]
    assert SCRIPT.main(args) == 0


def test_real_tauri_signature_rejects_a_changed_payload(
    tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    root = _write(tmp_path, TAURI_PAYLOAD + b"x", TAURI_SIGNATURE, "payload.bin")
    args = ["--config", str(_config(tmp_path, TAURI_PUBKEY)), str(root)]
    assert SCRIPT.main(args) == 1
    assert "signature does not verify" in capsys.readouterr().err


def test_repo_pubkey_parses() -> None:
    config = json.loads((REPO_ROOT / "src-tauri" / "tauri.conf.json").read_text())
    key = SCRIPT.parse_public_key(config["plugins"]["updater"]["pubkey"])
    assert key.key_id[::-1].hex().upper() == "B70253D0A2E576BA"


def test_legacy_unhashed_signature_verifies(tmp_path: Path) -> None:
    private = Ed25519PrivateKey.generate()
    signature = _signature(private, b"payload", algorithm=b"Ed")
    key = SCRIPT.parse_public_key(_pubkey(private))
    assert SCRIPT.verify_directory(_write(tmp_path, b"payload", signature), key) == []


@pytest.mark.parametrize(
    ("signature", "problem"),
    [
        (
            lambda p: _signature(Ed25519PrivateKey.generate(), b"payload"),
            "signature does not verify",
        ),
        (
            lambda p: _signature(p, b"payload", key_id=bytes(8)),
            "signed with key 0000000000000000, expected 0807060504030201",
        ),
        (
            lambda p: _signature(p, b"payload", algorithm=b"XX"),
            "unknown signature algorithm",
        ),
        (lambda p: "not base64!", "signature is not valid base64"),
        (lambda p: _b64(b"\xff\xfe"), "signature is not text"),
        (lambda p: _b64(b"one line\n"), "not a minisign signature file"),
        (
            lambda p: _b64(b"u\nAAAA\ntrusted comment: x\nAAAA\n"),
            "signature line has the wrong length",
        ),
        (
            # A valid signature with an edited trusted comment fails the
            # global signature.
            lambda p: _b64(
                base64.b64decode(_signature(p, b"payload")).replace(
                    b"timestamp:1", b"timestamp:2"
                )
            ),
            "signature does not verify",
        ),
    ],
    ids=[
        "other-key",
        "wrong-key-id",
        "unknown-algorithm",
        "not-base64",
        "not-text",
        "too-few-lines",
        "short-signature",
        "edited-trusted-comment",
    ],
)
def test_bad_signatures_are_reported(
    tmp_path: Path,
    signature: Callable[[Ed25519PrivateKey], str],
    problem: str,
) -> None:
    private = Ed25519PrivateKey.generate()
    key = SCRIPT.parse_public_key(_pubkey(private))
    root = _write(tmp_path, b"payload", signature(private))
    problems = SCRIPT.verify_directory(root, key)
    assert len(problems) == 1 and problem in problems[0]


def test_signature_without_payload_is_reported(tmp_path: Path) -> None:
    private = Ed25519PrivateKey.generate()
    root = _write(tmp_path, b"payload", _signature(private, b"payload"))
    (root / "app.deb" / "app.deb").unlink()
    problems = SCRIPT.verify_directory(root, SCRIPT.parse_public_key(_pubkey(private)))
    assert problems == ["app.deb.sig: no payload file next to it"]


def test_directory_without_signatures_is_reported(tmp_path: Path) -> None:
    key = SCRIPT.parse_public_key(_pubkey(Ed25519PrivateKey.generate()))
    assert SCRIPT.verify_directory(tmp_path, key) == [f"no .sig files under {tmp_path}"]


@pytest.mark.parametrize(
    "config",
    [
        None,
        "not json",
        json.dumps({"plugins": {}}),
        json.dumps({"plugins": {"updater": {"pubkey": _b64(b"")}}}),
        json.dumps({"plugins": {"updater": {"pubkey": _b64(b"c\nAAAA\n")}}}),
    ],
    ids=["missing-file", "not-json", "no-pubkey", "empty-key", "short-key"],
)
def test_unreadable_pubkey_fails(
    tmp_path: Path, config: str | None, capsys: pytest.CaptureFixture[str]
) -> None:
    path = tmp_path / "tauri.conf.json"
    if config is not None:
        path.write_text(config)
    assert SCRIPT.main(["--config", str(path), str(tmp_path)]) == 1
    assert "could not read the updater pubkey" in capsys.readouterr().err


def test_entry_point_exits_with_mains_status(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    root = _write(tmp_path, TAURI_PAYLOAD, TAURI_SIGNATURE, "payload.bin")
    config = _config(tmp_path, TAURI_PUBKEY)
    argv = [str(SCRIPT_PATH), "--config", str(config), str(root)]
    monkeypatch.setattr(sys, "argv", argv)
    with pytest.raises(SystemExit) as exc:
        runpy.run_path(str(SCRIPT_PATH), run_name="__main__")
    assert exc.value.code == 0
