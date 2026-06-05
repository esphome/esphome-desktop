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
      sed -n '2,40p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
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

# Inside-out signing: dylibs/.so first, then other Mach-O executables, then any
# nested framework/app bundles, then the top-level .app last. Mirrors the order
# in sign_python_bundle.sh so the embedded Python bundle validates.
sign_app() {
  local app="$1"
  echo "Signing app bundle: $app"
  # Drop any quarantine flag so Gatekeeper won't block the freshly signed app.
  # Stock macOS xattr has no recursive flag, so walk the tree ourselves.
  while IFS= read -r -d '' p; do
    xattr -d com.apple.quarantine "$p" 2>/dev/null || true
  done < <(find "$app" -print0)

  local count=0
  echo "  - shared libraries (.dylib/.so)"
  while IFS= read -r -d '' f; do
    sign_one "$f"; count=$((count + 1))
  done < <(find -L "$app" \( -name '*.dylib' -o -name '*.so' \) -type f -print0)

  echo "  - other Mach-O executables"
  while IFS= read -r -d '' f; do
    if file "$f" | grep -q "Mach-O"; then
      sign_one "$f"; count=$((count + 1))
    fi
  done < <(find -L "$app" -type f ! -name '*.dylib' ! -name '*.so' -print0)

  echo "  - nested framework/app bundles"
  while IFS= read -r -d '' d; do
    sign_one "$d"; count=$((count + 1))
  done < <(find "$app" -mindepth 1 \( -name '*.framework' -o -name '*.app' \) -type d -print0)

  echo "  - bundle root"
  sign_one "$app"; count=$((count + 1))
  echo "  signed $count items; verifying..."
  codesign --verify --deep --strict "$app"
  echo "  OK"
}

case "$INPUT" in
  *.app)
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
