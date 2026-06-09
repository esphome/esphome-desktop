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
# Upstream: tauri-apps/tauri#14686 (detach attempts not configurable) and
# tauri-apps/tauri#3055 (bundle_dmg.sh "failed to bundle project").
#
# The failure is transient, so re-running almost always succeeds. The Rust
# compile is cached (Swatinem/rust-cache), so a retry only re-runs the bundle
# step. We retry only when the failure looks like the DMG/hdiutil flake and
# fail fast on anything else (a compile, config, or signing error is
# deterministic and should surface immediately), and between attempts we scrub
# any scratch volume/image a wedged attempt left behind so it can't keep the
# device busy on the next try.
#
# Usage: ./build-scripts/macos_build_retry.sh [args for `cargo tauri build`]
# Paths are derived from the script location, so the working directory does
# not matter.

set -euo pipefail

ATTEMPTS=3
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TARGET_DIR="$REPO_ROOT/src-tauri/target"

# `cargo tauri build` resolves src-tauri relative to the working directory.
cd "$REPO_ROOT"

build_log="$(mktemp -t macos_build_retry)"
trap 'rm -f "$build_log"' EXIT

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
  done || true   # best effort: never let cleanup abort the retry
}

for ((attempt = 1; attempt <= ATTEMPTS; attempt++)); do
  # Capture cargo's own exit code (PIPESTATUS[0]), independent of tee, so a tee
  # write failure can't mask a real cargo result. Drop errexit for the pipeline
  # since a non-zero build is expected and handled explicitly below.
  set +e
  cargo tauri build "$@" 2>&1 | tee "$build_log"
  status="${PIPESTATUS[0]}"
  set -e
  if (( status == 0 )); then
    exit 0
  fi

  # Only the DMG bundling stage flakes. Anything else is deterministic, so fail
  # fast instead of paying the build cost two more times.
  if ! grep -qiE 'bundle_dmg|hdiutil|resource busy|failed to bundle' "$build_log"; then
    echo "cargo tauri build failed with a non-DMG error; not retrying (exit ${status})" >&2
    exit "$status"
  fi

  if (( attempt == ATTEMPTS )); then
    echo "DMG bundling failed after ${ATTEMPTS} attempts (exit ${status})" >&2
    exit "$status"
  fi

  echo "::warning::DMG bundling failed (attempt ${attempt}/${ATTEMPTS}); cleaning up stale disk images and retrying" >&2
  detach_orphaned_images
  find "$TARGET_DIR" -type f -name 'rw.*.dmg' -delete 2>/dev/null || true
  # Give the indexer a moment to release any remaining handles.
  sleep 5
done
