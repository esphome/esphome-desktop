#!/bin/bash
# Sign all Mach-O binaries in the Python bundle for macOS notarization
#
# This must run AFTER prepare_bundle.sh and BEFORE cargo tauri build.
# All executables, dylibs, and .so files need to be individually signed
# with hardened runtime and secure timestamps for Apple notarization.
#
# Required environment variable:
#   APPLE_SIGNING_IDENTITY - The Developer ID identity to sign with

set -e

if [[ -z "$APPLE_SIGNING_IDENTITY" ]]; then
    echo "APPLE_SIGNING_IDENTITY is not set, skipping bundle signing"
    exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUNDLE_DIR="$PROJECT_DIR/src-tauri/python"
ENTITLEMENTS="$SCRIPT_DIR/entitlements.plist"

if [[ ! -d "$BUNDLE_DIR" ]]; then
    echo "Error: Python bundle not found at $BUNDLE_DIR"
    echo "Run prepare_bundle.sh first."
    exit 1
fi

echo "=== Signing Python bundle binaries ==="
echo "Identity: $APPLE_SIGNING_IDENTITY"
echo "Entitlements: $ENTITLEMENTS"
echo ""

SIGNED=0

# Find all Mach-O files (executables, dylibs, .so files)
# Sign dylibs and .so files first (inside-out signing)
echo "--- Signing shared libraries (.dylib and .so) ---"
while IFS= read -r -d '' file; do
    echo "Signing: ${file#$BUNDLE_DIR/}"
    codesign --force --options runtime --timestamp \
        --entitlements "$ENTITLEMENTS" \
        --sign "$APPLE_SIGNING_IDENTITY" "$file"
    SIGNED=$((SIGNED + 1))
done < <(find "$BUNDLE_DIR" \( -name "*.dylib" -o -name "*.so" \) -print0)

# Sign executables in bin/
echo ""
echo "--- Signing executables ---"
while IFS= read -r -d '' file; do
    # Only sign Mach-O binaries, skip scripts
    if file "$file" | grep -q "Mach-O"; then
        echo "Signing: ${file#$BUNDLE_DIR/}"
        codesign --force --options runtime --timestamp \
            --entitlements "$ENTITLEMENTS" \
            --sign "$APPLE_SIGNING_IDENTITY" "$file"
        SIGNED=$((SIGNED + 1))
    fi
done < <(find "$BUNDLE_DIR/bin" -type f -print0)

echo ""
echo "=== Signed $SIGNED binaries ==="
