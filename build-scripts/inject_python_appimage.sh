#!/bin/bash
# Inject the Python bundle (and the tray's appindicator library) into a built
# AppImage and repack it.
#
# The sharun-based AppImage format uses DwarFS and does not include Tauri
# resources. `cargo tauri build` leaves the AppDir intact, so this copies
# Python into it and repacks with mkdwarfs + the uruntime header, in place.
# Release builds make the AppImage's updater .sig afterwards, from this final
# file, in the sign-linux job of .github/workflows/bundle-sign-upload.yml.
#
# Usage: ./build-scripts/inject_python_appimage.sh <path to .AppImage>
#
# The AppImage path is resolved from the repository root, which this script
# changes into. GITHUB_TOKEN (optional) authenticates the GitHub API lookup
# that cross-checks the mkdwarfs download.

set -euo pipefail

if (( $# != 1 )); then
  echo "usage: $0 <path to .AppImage>" >&2
  exit 2
fi
APPIMAGE="$1"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$(dirname "$SCRIPT_DIR")"

APPDIR="src-tauri/target/release/bundle/appimage/ESPHome Device Builder.AppDir"
echo "Found AppImage: $APPIMAGE"
echo "AppDir: $APPDIR"

# Copy Python bundle into the resource directory that
# get_bundled_resource_dir() resolves on Linux: exe_dir/../lib/esphome-desktop/
# The binary lives at bin/esphome-desktop, so this becomes lib/esphome-desktop/.
RESOURCE_DIR="$APPDIR/lib/esphome-desktop"
mkdir -p "$RESOURCE_DIR"
cp -r src-tauri/python "$RESOURCE_DIR/python"

# Bundle libayatana-appindicator3 and its runtime deps.
# The tray backend (libappindicator-sys) dlopen()s this library
# lazily by bare soname at runtime, so it is absent from the
# binary's DT_NEEDED and sharun/lib4bin never bundles it. Without
# it the tray silently disappears on the AppImage even on desktops
# that support it, e.g. KDE Plasma / GNOME (issue #87). Copy the
# library the build runner installed (libayatana-appindicator3-dev)
# plus its closure into sharun's lib dir, which is already on the
# loader search path and where platform/linux.rs's AppDir fallback
# looks for it.
LIB_DIR="$APPDIR/shared/lib"
mkdir -p "$LIB_DIR"

APPINDICATOR=""
for soname in libayatana-appindicator3.so.1 libappindicator3.so.1; do
  found=$(ldconfig -p | awk -v s="$soname" '$1 == s { print $NF; exit }')
  if [ -n "$found" ] && [ -e "$found" ]; then
    APPINDICATOR="$found"
    break
  fi
done
if [ -z "$APPINDICATOR" ]; then
  echo "::error::appindicator library not found on build runner; tray would be missing from the AppImage (issue #87)"
  exit 1
fi
echo "Bundling appindicator from: $APPINDICATOR"

# Copy the library plus every non-core object in its dependency
# closure into LIB_DIR, but never clobber a library sharun already
# bundled (e.g. the gtk/glib the webkit stack pulls in). Skip the
# glibc/loader core, which must come from the host to stay
# ABI-consistent. ldd already resolves the closure recursively, so a
# single pass is enough.
copy_dep() {
  local src="$1" base soname
  base=$(basename "$src")
  case "$base" in
    ld-linux*|libc.so*|libm.so*|libdl.so*|libpthread.so*|librt.so*|libresolv.so*)
      return 0 ;;
  esac
  # Name the bundled file after the ELF SONAME, not the on-disk
  # filename. ldconfig/ldd may resolve to a fully-versioned real file
  # (e.g. libfoo.so.1.2.3), but the loader (and the dlopen() by bare
  # soname described above) searches by SONAME (libfoo.so.1). Bundle
  # under the SONAME so runtime resolution finds it; fall back to the
  # basename only if the object has no SONAME.
  soname=$(patchelf --print-soname "$src" 2>/dev/null || true)
  [ -n "$soname" ] && base="$soname"
  # Skip if sharun already bundled this lib anywhere under shared/,
  # including the multiarch subdirs (shared/lib/x86_64-linux-gnu, seen
  # in the wild per platform/linux.rs): copying a second host-sourced
  # copy risks a version-skewed duplicate of a lib the app loads.
  [ -n "$(find "$APPDIR/shared" -name "$base" -print -quit 2>/dev/null)" ] && return 0
  cp -L "$src" "$LIB_DIR/$base"
  # Resolve co-bundled deps relative to the library's own directory.
  # patchelf is installed by linux-tauri-deps; a failure here would
  # ship a lib that resolves against the host and silently regress the
  # tray, so fail the build loud rather than continue.
  # shellcheck disable=SC2016  # a literal $ORIGIN, expanded by the loader
  if ! patchelf --set-rpath '$ORIGIN' "$LIB_DIR/$base"; then
    echo "::error::patchelf --set-rpath failed for $base; refusing to ship an AppImage whose bundled libs resolve against the host (issue #87)"
    exit 1
  fi
}

copy_dep "$APPINDICATOR"
# Captured first rather than read through process substitution, whose exit
# status set -e never sees: an ldd failure would otherwise ship the AppImage
# without the tray library's dependencies.
if ! ldd_out=$(ldd "$APPINDICATOR"); then
  echo "::error::ldd failed for $APPINDICATOR"
  exit 1
fi
deps=$(awk '/=> \// { print $3 }' <<< "$ldd_out")
while read -r dep; do
  [ -n "$dep" ] && [ -e "$dep" ] && copy_dep "$dep"
done <<< "$deps"

# Extract the uruntime header from the existing AppImage
chmod +x "$APPIMAGE"
OFFSET=$("$APPIMAGE" --appimage-offset)
dd if="$APPIMAGE" of=runtime.bin bs="$OFFSET" count=1

# Download mkdwarfs (same version used by quick-sharun.sh). The
# release asset name uses uname -m verbatim (x86_64 / aarch64).
# mkdwarfs repacks the AppImage shipped to every Linux user, so a
# tampered or truncated download would corrupt the published artifact.
# The dwarfs project publishes no SHA256SUMS asset, so pin the known
# hash per arch and verify before trusting the binary. --fail stops a
# saved HTTP error page; --retry rides out transient network blips.
DWARFS_VERSION="0.14.1"
DWARFS_ARCH=$(uname -m)
case "$DWARFS_ARCH" in
  x86_64)  DWARFS_SHA256="f3a117fd6d5b7304944b199af7fdb8086a48c509ea2e9832255d8f9a54c98587" ;;
  aarch64) DWARFS_SHA256="3a47eda6d496ba12fb6d9af1b1914d4ba7ab73df8df3172ee3cad66c29822cc1" ;;
  *) echo "::error::no pinned mkdwarfs SHA256 for arch '$DWARFS_ARCH'"; exit 1 ;;
esac
DWARFS_ASSET="dwarfs-universal-${DWARFS_VERSION}-Linux-${DWARFS_ARCH}"
DWARFS_URL="https://github.com/mhx/dwarfs/releases/download/v${DWARFS_VERSION}/${DWARFS_ASSET}"
curl -fL --retry 3 --retry-delay 2 -o mkdwarfs "$DWARFS_URL"
echo "${DWARFS_SHA256}  mkdwarfs" | sha256sum -c -

# Cross-check against the sha256 digest GitHub records for the release
# asset. This is defense in depth on top of the
# inline pin: it catches a stale pin after a version bump, while the
# pin still guards against a compromised asset (the API would report
# the bad asset's digest, so it can't replace the pin). Older releases
# may predate GitHub's asset-digest feature, so a missing digest only
# warns rather than failing the build. The pin already verified the
# binary above, so a transient API failure must not red an
# already-correct build: --retry rides out blips and the trailing
# `|| true` degrades a hard failure to the no-digest warning branch.
AUTH_HEADER=()
if [ -n "${GITHUB_TOKEN:-}" ]; then
  AUTH_HEADER=(-H "Authorization: Bearer ${GITHUB_TOKEN}")
fi
API_DIGEST=$(curl -fsSL --retry 3 --retry-delay 2 \
  -H "Accept: application/vnd.github+json" \
  "${AUTH_HEADER[@]}" \
  "https://api.github.com/repos/mhx/dwarfs/releases/tags/v${DWARFS_VERSION}" \
  | jq -r --arg name "$DWARFS_ASSET" \
      '.assets[] | select(.name == $name) | .digest // empty' || true)
if [ -n "$API_DIGEST" ]; then
  echo "${API_DIGEST#sha256:}  mkdwarfs" | sha256sum -c -
  echo "Verified mkdwarfs against GitHub API digest: $API_DIGEST"
else
  echo "::warning::GitHub API reported no digest for ${DWARFS_ASSET}; relying on pinned SHA256"
fi
chmod +x mkdwarfs

# Repack: create DwarFS image from the AppDir and combine with runtime
./mkdwarfs -i "$APPDIR" -o new.dwarfs --no-history -l7
cat runtime.bin new.dwarfs > "$APPIMAGE"
chmod +x "$APPIMAGE"

# Show the result
echo "AppImage size: $(du -sh "$APPIMAGE" | cut -f1)"

# Clean up
rm -f runtime.bin new.dwarfs mkdwarfs
