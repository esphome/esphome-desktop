#!/usr/bin/env python3
"""Verify every Tauri updater .sig in an artifact directory against the pubkey.

Tauri only warns when the key it signs with does not match the updater pubkey
in tauri.conf.json, and an installed app rejects an update whose signature
does not verify. So before anything is uploaded, upload-release-assets in
bundle-sign-upload.yml checks each .sig the way the updater will: the
signature must come from the configured key and cover the exact payload file
it ships next to.

The formats are minisign's. The pubkey setting and each .sig file are base64
of a minisign text file. A public key line decodes to the algorithm (`Ed`),
an 8-byte key ID and a 32-byte Ed25519 key. A signature line decodes to the
algorithm (`ED` for a BLAKE2b-512 prehash of the file, `Ed` for the legacy
unhashed form), the key ID and a 64-byte signature; the global signature
then covers that signature plus the trusted comment.

Usage:
  python verify_updater_signatures.py --config src-tauri/tauri.conf.json DIR
"""

from __future__ import annotations

import argparse
import base64
import binascii
import hashlib
import json
import sys
from dataclasses import dataclass
from pathlib import Path

from cryptography.exceptions import InvalidSignature
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

_SCRIPT_DIR = str(Path(__file__).resolve().parent)
if _SCRIPT_DIR not in sys.path:
    sys.path.insert(0, _SCRIPT_DIR)

from _gha import error  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG = REPO_ROOT / "src-tauri" / "tauri.conf.json"
TRUSTED_PREFIX = "trusted comment: "
CHUNK_SIZE = 1 << 20


class SignatureError(Exception):
    """A signature or key could not be parsed or did not verify."""


@dataclass(frozen=True)
class PublicKey:
    key_id: bytes
    key: Ed25519PublicKey


def _decode(data: str, what: str) -> bytes:
    try:
        return base64.b64decode(data.strip(), validate=True)
    except binascii.Error as exc:
        raise SignatureError(f"{what} is not valid base64") from exc


def _minisign_lines(encoded: str, what: str) -> list[str]:
    """Decode a Tauri-style base64 minisign file into its non-empty lines."""
    try:
        text = _decode(encoded, what).decode("utf-8")
    except UnicodeDecodeError as exc:
        raise SignatureError(f"{what} is not text") from exc
    return [line for line in text.splitlines() if line.strip()]


def parse_public_key(encoded: str) -> PublicKey:
    """Parse the `plugins.updater.pubkey` value from tauri.conf.json."""
    lines = _minisign_lines(encoded, "updater pubkey")
    raw = _decode(lines[-1], "updater pubkey line") if lines else b""
    if len(raw) != 42 or raw[:2] != b"Ed":
        raise SignatureError("updater pubkey is not a minisign Ed25519 key")
    return PublicKey(raw[2:10], Ed25519PublicKey.from_public_bytes(raw[10:]))


def _digest(path: Path) -> bytes:
    hasher = hashlib.blake2b(digest_size=64)
    with path.open("rb") as fh:
        while chunk := fh.read(CHUNK_SIZE):
            hasher.update(chunk)
    return hasher.digest()


def verify(signature_file: str, payload: Path, public_key: PublicKey) -> None:
    """Raise unless signature_file is a valid signature of payload by the key."""
    lines = _minisign_lines(signature_file, "signature")
    if len(lines) != 4 or not lines[2].startswith(TRUSTED_PREFIX):
        raise SignatureError("signature is not a minisign signature file")
    raw = _decode(lines[1], "signature line")
    if len(raw) != 74:
        raise SignatureError("signature line has the wrong length")
    algorithm, key_id, signature = raw[:2], raw[2:10], raw[10:]
    if key_id != public_key.key_id:
        raise SignatureError(
            f"signed with key {key_id[::-1].hex().upper()}, expected "
            f"{public_key.key_id[::-1].hex().upper()}"
        )
    if algorithm == b"ED":
        message = _digest(payload)
    elif algorithm == b"Ed":
        message = payload.read_bytes()
    else:
        raise SignatureError(f"unknown signature algorithm {algorithm!r}")
    trusted = lines[2][len(TRUSTED_PREFIX) :].encode("utf-8")
    global_signature = _decode(lines[3], "global signature")
    try:
        public_key.key.verify(signature, message)
        public_key.key.verify(global_signature, signature + trusted)
    except InvalidSignature as exc:
        raise SignatureError("signature does not verify") from exc


def verify_directory(directory: Path, public_key: PublicKey) -> list[str]:
    """Verify every .sig under directory; return a problem per failure."""
    files = {p.name: p for p in directory.rglob("*") if p.is_file()}
    signatures = sorted(name for name in files if name.endswith(".sig"))
    problems: list[str] = []
    if not signatures:
        problems.append(f"no .sig files under {directory}")
    for name in signatures:
        payload = files.get(name[: -len(".sig")])
        if payload is None:
            problems.append(f"{name}: no payload file next to it")
            continue
        try:
            verify(files[name].read_text(encoding="utf-8"), payload, public_key)
        except (OSError, UnicodeDecodeError, SignatureError) as exc:
            problems.append(f"{name}: {exc}")
        else:
            print(f"Verified {name}")
    return problems


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG)
    parser.add_argument("directory", type=Path)
    args = parser.parse_args(argv)

    try:
        config = json.loads(args.config.read_text(encoding="utf-8"))
        public_key = parse_public_key(config["plugins"]["updater"]["pubkey"])
    except (OSError, ValueError, KeyError, TypeError, SignatureError) as exc:
        error(f"could not read the updater pubkey from {args.config}: {exc}")
        return 1
    problems = verify_directory(args.directory, public_key)
    for problem in problems:
        error(problem)
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
