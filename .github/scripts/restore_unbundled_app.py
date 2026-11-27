#!/usr/bin/env python3
"""Unpack a release build's unbundled-app tarball into src-tauri/, safely.

build.yml's release legs tar up the compiled app plus the python/, git/ and
ccache/ resource trees; bundle-sign-upload.yml's bundle jobs unpack it and then
run `cargo tauri bundle` with the signing credentials present. The build job
runs third-party code (pip installs, crate build scripts), so the tarball is
untrusted input to a credentialed job. Anything it could place outside the
expected paths would be read by the bundler or Tauri with those credentials in
reach: a replacement tauri.*.conf.json (a new signCommand or
beforeBundleCommand), a .cargo/config.toml, or a symlink that pulls a file such
as an Azure token cache into the signed installer.

So every member must be the app binary itself or sit inside one of the
resource trees, be a plain file, directory or link, and never sit beneath a
symlink. Any symlink must resolve inside its own tree, followed through the
archive's other symlinks as the filesystem will follow them, and any hard
link must name a file in its tree. Names must be unique. All of these
comparisons fold case and Unicode normalisation, as the macOS and Windows
filesystems do. The archive is rejected as a whole, before anything is
written, if one member fails.

Those checks model the filesystem, so they are not the last word. The
archive is unpacked into an empty staging directory outside src-tauri/
(tarfile's `data` filter keeps every write inside it, and also strips
setuid/setgid bits), every unpacked entry is then checked with realpath on
the real filesystem, and only after that are the binary and the trees moved
into src-tauri/.

Python's tarfile rather than the platform tar: Git Bash's GNU tar on Windows
reads the `D:` of a runner path as a remote host name.

Usage:
  python restore_unbundled_app.py --tarball-dir DIR --binary REL_PATH [--dest DIR]
"""

from __future__ import annotations

import argparse
import os
import posixpath
import shutil
import sys
import tarfile
import tempfile
import unicodedata
from pathlib import Path

_SCRIPT_DIR = str(Path(__file__).resolve().parent)
if _SCRIPT_DIR not in sys.path:
    sys.path.insert(0, _SCRIPT_DIR)

from _gha import error  # noqa: E402

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DEST = REPO_ROOT / "src-tauri"
# The resource trees prepare_bundle.sh builds, relative to src-tauri/.
RESOURCE_TREES = ("python", "git", "ccache")
# Matches Linux's limit on symlinks followed while resolving one path.
MAX_SYMLINK_HOPS = 40


class UnsafeArchiveError(Exception):
    """The tarball holds a member the bundle job must not unpack."""


def _normalise(name: str) -> str:
    """Return a member name without a leading './' or trailing '/'."""
    while name.startswith("./"):
        name = name[2:]
    return name.rstrip("/")


def _tree_of(name: str) -> str | None:
    """Return the resource tree a member path sits in, if any."""
    top = name.split("/", 1)[0]
    return top if top in RESOURCE_TREES else None


def _check_path(name: str, binary: str) -> str | None:
    """Validate one member path; return its resource tree (None for the binary)."""
    if not name or name.startswith("/") or "\\" in name:
        raise UnsafeArchiveError(f"unexpected member path: {name!r}")
    if any(part in ("", ".", "..") for part in name.split("/")):
        raise UnsafeArchiveError(f"non-canonical member path: {name!r}")
    if name == binary:
        return None
    tree = _tree_of(name)
    if tree is None:
        raise UnsafeArchiveError(
            f"member outside the app binary and resource trees: {name!r}"
        )
    return tree


def _fold(name: str) -> str:
    """Return the key two names share if the filesystem treats them as one.

    macOS (APFS) and Windows (NTFS) runners compare names case-insensitively,
    and APFS also ignores Unicode normalisation, so `L` and `l` are the same
    entry there. Every comparison between member names goes through this.
    """
    return unicodedata.normalize("NFC", name).casefold()


def _within(path: str, tree: str) -> bool:
    return path == tree or path.startswith(tree + "/")


def _resolve(path: str, symlinks: dict[str, str]) -> str:
    """Resolve a path the way the filesystem will, through the archive's symlinks.

    A lexical normpath is not enough: `python/a -> ../..` is lexically inside
    python/, but a later link through it (`python/a/b -> ..`) lands above it.
    So each component is looked up in the archive's own symlinks (keyed by
    `_fold`), as the OS will once they exist on disk. Returns the folded
    path. Raises if the path climbs above src-tauri/ or the links loop.
    """
    resolved: list[str] = []
    pending = path.split("/")
    hops = 0
    while pending:
        part = pending.pop(0)
        if part in ("", "."):
            continue
        if part == "..":
            if not resolved:
                raise UnsafeArchiveError(f"path {path!r} climbs out of src-tauri/")
            resolved.pop()
            continue
        part = _fold(part)
        candidate = "/".join([*resolved, part])
        target = symlinks.get(candidate)
        if target is None:
            resolved.append(part)
            continue
        hops += 1
        if hops > MAX_SYMLINK_HOPS:
            raise UnsafeArchiveError(f"symlink loop resolving {path!r}")
        # A symlink's target is relative to the directory holding it, which is
        # exactly what `resolved` holds at this point.
        pending[:0] = target.split("/")
    return "/".join(resolved)


def _check_link(
    name: str,
    tree: str | None,
    member: tarfile.TarInfo,
    symlinks: dict[str, str],
    files: set[str],
) -> None:
    """Check a symlink resolves inside its tree and a hard link names a file there."""
    if tree is None:
        raise UnsafeArchiveError(f"the app binary must be a regular file: {name!r}")
    target = member.linkname
    if not target or target.startswith("/") or "\\" in target:
        raise UnsafeArchiveError(f"link {name!r} has an unsafe target {target!r}")
    if member.issym():
        resolved = _resolve(posixpath.join(posixpath.dirname(name), target), symlinks)
        ok = _within(resolved, tree)
    else:
        # tarfile extracts a hard link as a copy of the archive member it
        # names, so that member must be a regular file in the same tree.
        resolved = _fold(_normalise(target))
        ok = resolved in files and _within(resolved, tree)
    if not ok:
        raise UnsafeArchiveError(f"link {name!r} -> {target!r} points outside {tree}/")


def check_members(members: list[tarfile.TarInfo], binary: str) -> None:
    """Reject the archive unless every member is expected and safe."""
    names = [_normalise(member.name) for member in members]
    folded = [_fold(name) for name in names]
    if len(set(folded)) != len(folded):
        # A later member of the same name replaces the earlier one on
        # extraction, so a checked file could be swapped for a link.
        raise UnsafeArchiveError("duplicate member names")
    symlinks = {
        key: member.linkname
        for key, member in zip(folded, members, strict=True)
        if member.issym()
    }
    files = {
        key for key, member in zip(folded, members, strict=True) if member.isfile()
    }

    found_binary = False
    for name, key, member in zip(names, folded, members, strict=True):
        tree = _check_path(name, binary)
        # Nothing may be written through a symlink: real trees store a link
        # and never members beneath it, and extracting through one would
        # place a file wherever the link points.
        parts = key.split("/")
        for depth in range(1, len(parts)):
            if "/".join(parts[:depth]) in symlinks:
                raise UnsafeArchiveError(f"member {name!r} sits beneath a symlink")
        if member.issym() or member.islnk():
            _check_link(name, tree, member, symlinks, files)
        elif member.isfile():
            found_binary = found_binary or tree is None
        elif not member.isdir() or tree is None:
            raise UnsafeArchiveError(f"unsupported member type: {name!r}")
    if not found_binary:
        raise UnsafeArchiveError(f"the app binary {binary!r} is missing")


def find_tarball(tarball_dir: Path) -> Path:
    """Return the single .tar file download-artifact left in a directory."""
    tarballs = sorted(p for p in tarball_dir.rglob("*.tar") if p.is_file())
    if len(tarballs) != 1:
        raise UnsafeArchiveError(
            f"expected exactly one tarball in {tarball_dir}, found {len(tarballs)}"
        )
    return tarballs[0]


def _inside(path: str, root: str) -> bool:
    return path == root or path.startswith(root + os.sep)


def check_extracted(staging: Path, binary: str) -> None:
    """Check the unpacked staging tree on the real filesystem.

    The member checks above model how the filesystem will resolve names; this
    asks the filesystem itself. Every entry's realpath, with every symlink
    followed as the OS follows it, must stay inside its own resource tree,
    and the binary must be a plain file. It runs before anything leaves the
    staging directory, so a model gap cannot reach src-tauri/.
    """
    allowed_top = {binary.split("/", 1)[0], *RESOURCE_TREES}
    for entry in staging.iterdir():
        if entry.name not in allowed_top:
            raise UnsafeArchiveError(f"unexpected entry after unpacking: {entry.name}")

    binary_path = staging / binary
    if binary_path.is_symlink() or not binary_path.is_file():
        raise UnsafeArchiveError(f"the app binary {binary!r} is not a regular file")
    real_staging = os.path.realpath(staging)
    if os.path.realpath(binary_path) != os.path.join(real_staging, *binary.split("/")):
        raise UnsafeArchiveError(f"the app binary {binary!r} resolves elsewhere")

    for tree in RESOURCE_TREES:
        root = staging / tree
        if not root.exists() and not root.is_symlink():
            continue
        if root.is_symlink() or not root.is_dir():
            raise UnsafeArchiveError(f"{tree}/ is not a directory")
        real_root = os.path.realpath(root)
        for dirpath, dirnames, filenames in os.walk(root, followlinks=False):
            for name in [*dirnames, *filenames]:
                path = os.path.join(dirpath, name)
                if not _inside(os.path.realpath(path), real_root):
                    relative = os.path.relpath(path, staging)
                    raise UnsafeArchiveError(f"{relative} resolves outside {tree}/")


def _install(staging: Path, binary: str, dest: Path) -> None:
    """Move the checked binary and resource trees from staging into dest."""
    for tree in RESOURCE_TREES:
        target = dest / tree
        if target.exists() or target.is_symlink():
            raise UnsafeArchiveError(f"{target} already exists")
    binary_target = dest / binary
    if binary_target.exists() or binary_target.is_symlink():
        raise UnsafeArchiveError(f"{binary_target} already exists")

    binary_target.parent.mkdir(parents=True, exist_ok=True)
    shutil.move(staging / binary, binary_target)
    for tree in RESOURCE_TREES:
        if (staging / tree).is_dir():
            shutil.move(staging / tree, dest / tree)


def restore(
    tarball: Path, binary: str, dest: Path, staging_parent: Path | None = None
) -> int:
    """Validate and unpack a tarball; return the number of members written.

    The archive is unpacked into a fresh directory outside dest, so tarfile's
    `data` filter (which keeps every write inside its destination) confines
    it to that directory rather than to src-tauri/. Only once the result
    passes check_extracted are the binary and the trees moved into dest.
    """
    binary = _normalise(binary)
    with tarfile.open(tarball, "r:") as archive:
        members = archive.getmembers()
        check_members(members, binary)
        with tempfile.TemporaryDirectory(
            prefix="unbundled-", dir=staging_parent
        ) as tmp:
            staging = Path(tmp)
            archive.extractall(staging, members=members, filter="data")
            check_extracted(staging, binary)
            _install(staging, binary, dest)
    return len(members)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--tarball-dir", type=Path, required=True)
    parser.add_argument(
        "--binary",
        required=True,
        help="App binary path relative to src-tauri/, as the build job packed it.",
    )
    parser.add_argument("--dest", type=Path, default=DEFAULT_DEST)
    args = parser.parse_args(argv)

    # Stage on the runner's temp volume: on Windows the default temp dir is
    # on another drive from the workspace, which turns the move into a copy.
    runner_temp = os.environ.get("RUNNER_TEMP")
    staging_parent = Path(runner_temp) if runner_temp else None
    try:
        tarball = find_tarball(args.tarball_dir)
        count = restore(tarball, args.binary, args.dest, staging_parent)
    except (OSError, tarfile.TarError, UnsafeArchiveError) as exc:
        error(f"refusing to restore the unbundled app: {exc}")
        return 1
    print(f"Unpacked {count} members of {tarball.name} into {args.dest}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
