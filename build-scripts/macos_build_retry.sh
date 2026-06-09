#!/bin/bash
# Retry wrapper for `cargo tauri build` on macOS CI.
#
# The DMG stage (Tauri's generated bundle_dmg.sh, a fork of create-dmg)
# intermittently fails on GitHub's macOS runners. It mounts a scratch
# read-write volume with hdiutil, decorates it, then detaches and compresses
# it. The detach frequently fails with "Resource busy" because Spotlight /
# fseventsd is still indexing the freshly mounted volume, and create-dmg only
# retries the detach a few times before giving up with a non-zero exit. This
# repo embeds a large Python bundle into the .app, so the image is big and the
# busy window is long, which widens the flake.
#
# The failure is transient, so re-running almost always succeeds. The Rust
# compile is cached (Swatinem/rust-cache), so a retry only re-runs the bundle
# step. Between attempts we scrub any scratch volume/image a wedged attempt
# left behind so it can't keep the device busy on the next try.
#
# Usage: ./macos_build_retry.sh [args passed through to `cargo tauri build`]

set -euo pipefail

ATTEMPTS=3
TARGET_DIR="$PWD/src-tauri/target"

# Detach any scratch disk images that a failed attempt left mounted. create-dmg
# names them rw.<pid>.<dmg> under the bundle/dmg directory, so we match on this
# repo's target path and detach the backing device (best effort).
detach_orphaned_images() {
  hdiutil info 2>/dev/null | awk -v dir="$TARGET_DIR" '
    function flush() {
      if (dev != "" && index(path, dir) > 0) print dev
      path = ""; dev = ""
    }
    /^====/ { flush(); next }
    /^image-path/ { path = $0; next }
    /^\/dev\/disk/ { if (dev == "") dev = $1 }
    END { flush() }
  ' | while read -r dev; do
    [[ -n "$dev" ]] && hdiutil detach "$dev" -force >/dev/null 2>&1 || true
  done
}

for ((attempt = 1; attempt <= ATTEMPTS; attempt++)); do
  status=0
  cargo tauri build "$@" || status=$?
  if (( status == 0 )); then
    exit 0
  fi

  if (( attempt == ATTEMPTS )); then
    echo "cargo tauri build failed after ${ATTEMPTS} attempts (exit ${status})" >&2
    exit "$status"
  fi

  echo "::warning::cargo tauri build failed (attempt ${attempt}/${ATTEMPTS}); cleaning up stale disk images and retrying" >&2
  detach_orphaned_images
  find src-tauri/target -type f -name 'rw.*.dmg' -delete 2>/dev/null || true
  # Give the indexer a moment to release any remaining handles.
  sleep 5
done
