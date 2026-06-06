#!/bin/bash
# Re-sign a macOS .dmg (or .app) locally so the bundled app opens on your Mac.
#
# CI builds posted on PRs are NOT signed or notarized, so Gatekeeper refuses to
# open them. This re-signs every Mach-O binary inside the app (the embedded
# Python bundle, its dylibs/.so files, helper executables, then the .app itself)
# and re-seals the dmg, which is enough for the app to launch locally.
#
# By default it uses an ad-hoc signature (codesign --sign -), which needs no
# Apple Developer certificate and no network, and is all that's required to open
# the app on the machine you signed it on. If you have a Developer ID identity,
# pass --identity (or set APPLE_SIGNING_IDENTITY) to sign with that instead;
# that path also applies the hardened runtime and the project entitlements, the
# same way CI signs release builds.
#
# Usage:
#   build-scripts/sign_macos_local.sh <path-to.dmg|path-to.app> [options]
#
# Options:
#   -i, --identity <name>   Signing identity (e.g. "Developer ID Application: ...").
#                           Defaults to $APPLE_SIGNING_IDENTITY, else a single
#                           Developer ID found in your keychain, else ad-hoc (-).
#   -o, --output <path>     Output dmg path (dmg input only).
#                           Defaults to "<input>-signed.dmg".
#       --adhoc             Force ad-hoc signing even if an identity is available.
#       --no-timestamp      Skip the secure timestamp (offline; identity signing
#                           only). Ad-hoc never uses a timestamp.
#   -h, --help              Show this help.
#
# Examples:
#   build-scripts/sign_macos_local.sh ~/Downloads/"ESPHome Device Builder_0.9.1_aarch64.dmg"
#   APPLE_SIGNING_IDENTITY="Developer ID Application: Jane (TEAMID)" \
#     build-scripts/sign_macos_local.sh build.dmg

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENTITLEMENTS="$SCRIPT_DIR/entitlements.plist"

INPUT=""
IDENTITY="${APPLE_SIGNING_IDENTITY:-}"
OUTPUT=""
FORCE_ADHOC=0
NO_TIMESTAMP=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    -i|--identity) IDENTITY="$2"; shift 2 ;;
    -o|--output)   OUTPUT="$2"; shift 2 ;;
    --adhoc)       FORCE_ADHOC=1; shift ;;
    --no-timestamp) NO_TIMESTAMP=1; shift ;;
    -h|--help)
      # Print the header comment block (line 2 through the first non-# line) so
      # editing the header can't truncate help via a hardcoded line range.
      awk 'NR==1 {next} /^#/ {sub(/^# ?/, ""); print; next} {exit}' "${BASH_SOURCE[0]}"
      exit 0 ;;
    -*) echo "Unknown option: $1" >&2; exit 2 ;;
    *)  if [[ -n "$INPUT" ]]; then echo "Unexpected argument: $1" >&2; exit 2; fi
        INPUT="$1"; shift ;;
  esac
done

if [[ -z "$INPUT" ]]; then
  echo "Error: no input given. Pass a .dmg or .app path (try --help)." >&2
  exit 2
fi
if [[ ! -e "$INPUT" ]]; then
  echo "Error: no such file: $INPUT" >&2
  exit 1
fi

# Resolve the identity. Ad-hoc ("-") is the default so this works with no cert.
if [[ "$FORCE_ADHOC" == 1 ]]; then
  IDENTITY="-"
elif [[ -z "$IDENTITY" ]]; then
  # Auto-pick a Developer ID Application identity if exactly one is present.
  IDS=()
  while IFS= read -r line; do
    [[ -n "$line" ]] && IDS+=("$line")
  done < <(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)"/\1/p')
  if [[ "${#IDS[@]}" -eq 1 ]]; then
    IDENTITY="${IDS[0]}"
  else
    IDENTITY="-"
  fi
fi

if [[ "$IDENTITY" == "-" ]]; then
  echo "Signing identity: ad-hoc (-)  [opens locally; no cert or network needed]"
else
  echo "Signing identity: $IDENTITY"
fi

# Build the codesign flag list once; it's constant for the whole run. Ad-hoc
# stays minimal so the app just opens; a real identity gets the hardened runtime
# + entitlements like CI release builds.
CODESIGN_FLAGS=(--force --sign "$IDENTITY")
if [[ "$IDENTITY" != "-" ]]; then
  CODESIGN_FLAGS+=(--options runtime --entitlements "$ENTITLEMENTS")
  [[ "$NO_TIMESTAMP" == 1 ]] || CODESIGN_FLAGS+=(--timestamp)
fi

sign_one() {
  codesign "${CODESIGN_FLAGS[@]}" "$1"
}

# Fan signing out across cores. The embedded Python bundle has hundreds of
# Mach-O files, so signing them one process at a time is the slow part.
JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"

# Inside-out signing: dylibs/.so first, then other Mach-O executables, then any
# nested framework/app bundles, then the top-level .app last. Mirrors the order
# in sign_python_bundle.sh so the embedded Python bundle validates.
sign_app() {
  local app="$1"
  echo "Signing app bundle: $app  (up to $JOBS parallel workers)"

  # Clear extended attributes (including com.apple.quarantine) so Gatekeeper
  # won't block the freshly signed app. xattr -c never errors on a file that has
  # none, and the code signature lives in the binary / _CodeSignature, not in an
  # xattr, so this is safe to do before signing.
  find "$app" -print0 | xargs -0 -r -P "$JOBS" -n 200 xattr -c 2>/dev/null || true

  # Loose Mach-O code (dylibs, .so, helper executables) is independent, so it can
  # be signed in parallel and batched several files per codesign call. Only
  # bundles need inside-out ordering, so they (and the .app root) come afterward.
  echo "  - shared libraries (.dylib/.so)"
  local libs=()
  while IFS= read -r -d '' f; do libs+=("$f"); done \
    < <(find -L "$app" \( -name '*.dylib' -o -name '*.so' \) -type f -print0)
  if [[ "${#libs[@]}" -gt 0 ]]; then
    printf '%s\0' "${libs[@]}" | xargs -0 -r -P "$JOBS" -n 16 codesign "${CODESIGN_FLAGS[@]}"
  fi

  # Detect Mach-O among the remaining files in parallel (a `file` call per file
  # was a big chunk of the old runtime), then sign the matches batched.
  echo "  - other Mach-O executables"
  # The $f / $(file ...) below are written for the inner `sh -c`, not expanded
  # by this shell, so the SC2016 single-quote warning is expected.
  local machos=()
  # shellcheck disable=SC2016
  while IFS= read -r -d '' f; do machos+=("$f"); done < <(
    find -L "$app" -type f ! -name '*.dylib' ! -name '*.so' -print0 \
      | xargs -0 -r -P "$JOBS" -n 64 sh -c '
          for f; do
            case "$(file -b "$f")" in *Mach-O*) printf "%s\0" "$f" ;; esac
          done' sh
  )
  if [[ "${#machos[@]}" -gt 0 ]]; then
    printf '%s\0' "${machos[@]}" | xargs -0 -r -P "$JOBS" -n 16 codesign "${CODESIGN_FLAGS[@]}"
  fi

  # Nested bundles must be sealed after their contents, so sign them serially
  # and depth-first (-depth) so an inner bundle is signed before the bundle that
  # contains it.
  echo "  - nested framework/app bundles"
  while IFS= read -r -d '' d; do
    sign_one "$d"
  done < <(find "$app" -depth -mindepth 1 \( -name '*.framework' -o -name '*.app' \) -type d -print0)

  echo "  - bundle root"
  sign_one "$app"
  echo "  signed ${#libs[@]} libraries + ${#machos[@]} executables; verifying..."
  codesign --verify --deep --strict "$app"
  echo "  OK"
}

case "$INPUT" in
  *.app)
    if [[ -n "$OUTPUT" ]]; then
      echo "Error: --output applies to .dmg input only; a .app is signed in place." >&2
      exit 2
    fi
    sign_app "$INPUT"
    echo "Done. Signed app in place: $INPUT"
    ;;
  *.dmg)
    [[ -n "$OUTPUT" ]] || OUTPUT="${INPUT%.dmg}-signed.dmg"
    if [[ -e "$OUTPUT" ]]; then
      echo "Error: output already exists: $OUTPUT (use --output)" >&2
      exit 1
    fi
    WORK="$(mktemp -d)"
    MNT=""
    cleanup() {
      [[ -n "$MNT" && -d "$MNT" ]] && hdiutil detach "$MNT" -quiet 2>/dev/null || true
      rm -rf "$WORK"
    }
    trap cleanup EXIT

    echo "Converting to a writable image..."
    hdiutil convert "$INPUT" -format UDRW -o "$WORK/rw.dmg" -quiet
    # The source dmg is sized to its content with almost no slack. Signing adds
    # data that wasn't there (embedded Mach-O signatures plus a _CodeSignature
    # dir for every nested bundle); for the full Python bundle that's enough to
    # fill the volume and make codesign fail mid-run with a confusing write
    # error. Grow the writable image first (50% + 128 MB headroom); the UDZO
    # reseal drops the slack again, so the final dmg isn't bloated.
    cur_mb=$(( $(stat -f%z "$WORK/rw.dmg") / 1048576 ))
    hdiutil resize -size "$(( cur_mb + cur_mb / 2 + 128 ))m" "$WORK/rw.dmg" -quiet \
      || echo "Warning: could not grow the writable image; proceeding without extra headroom." >&2
    MNT="$WORK/mnt"
    mkdir -p "$MNT"
    echo "Mounting..."
    hdiutil attach "$WORK/rw.dmg" -readwrite -nobrowse -noverify -mountpoint "$MNT" -quiet

    APP="$(find "$MNT" -maxdepth 1 -name '*.app' -type d | head -1)"
    if [[ -z "$APP" ]]; then
      echo "Error: no .app found inside the dmg" >&2
      exit 1
    fi
    sign_app "$APP"

    echo "Unmounting..."
    hdiutil detach "$MNT" -quiet
    MNT=""

    echo "Re-sealing compressed image -> $OUTPUT"
    hdiutil convert "$WORK/rw.dmg" -format UDZO -o "$OUTPUT" -quiet
    # Signing the dmg container is only meaningful with a real identity.
    if [[ "$IDENTITY" != "-" ]]; then
      codesign --force --sign "$IDENTITY" "$OUTPUT"
    fi
    echo "Done. Signed dmg: $OUTPUT"
    echo "Mount it, drag the app to /Applications, and it will open."
    ;;
  *)
    echo "Error: input must be a .dmg or .app: $INPUT" >&2
    exit 2
    ;;
esac
