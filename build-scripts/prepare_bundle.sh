#!/bin/bash
# Prepare the Python environment with ESPHome pre-installed for bundling
#
# This script:
# 1. Downloads python-build-standalone
# 2. Creates a venv with ESPHome installed
# 3. Prepares it for bundling with the app
#
# Usage: ./prepare_bundle.sh [platform]

set -e

PYTHON_VERSION="3.13.0"
PBS_VERSION="20241016"
BASE_URL="https://github.com/indygreg/python-build-standalone/releases/download/${PBS_VERSION}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/build"
BUNDLE_DIR="$PROJECT_DIR/src-tauri/python"

detect_platform() {
    local os=$(uname -s)
    local arch=$(uname -m)

    case "$os" in
        Darwin)
            if [[ "$arch" == "arm64" ]]; then
                echo "macos-arm64"
            else
                echo "macos-x64"
            fi
            ;;
        Linux)
            echo "linux-x64"
            ;;
        MINGW*|MSYS*|CYGWIN*)
            echo "windows-x64"
            ;;
        *)
            echo "unknown"
            ;;
    esac
}

PLATFORM="${1:-$(detect_platform)}"

if [[ "$PLATFORM" == "unknown" ]]; then
    echo "Could not detect platform. Please specify: macos-x64, macos-arm64, windows-x64, linux-x64"
    exit 1
fi

# Set platform-specific variables
case $PLATFORM in
    macos-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-apple-darwin-install_only.tar.gz"
        PYTHON_BIN="bin/python3"
        VENV_PYTHON="bin/python"
        ;;
    macos-arm64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-aarch64-apple-darwin-install_only.tar.gz"
        PYTHON_BIN="bin/python3"
        VENV_PYTHON="bin/python"
        ;;
    windows-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-pc-windows-msvc-shared-install_only.tar.gz"
        PYTHON_BIN="python.exe"
        VENV_PYTHON="Scripts/python.exe"
        ;;
    linux-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-unknown-linux-gnu-install_only.tar.gz"
        PYTHON_BIN="bin/python3"
        VENV_PYTHON="bin/python"
        ;;
esac

URL="${BASE_URL}/${FILENAME}"
PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"
VENV_DIR="$BUILD_DIR/venv-${PLATFORM}"

echo "=== Preparing ESPHome bundle for ${PLATFORM} ==="

# Clean up previous builds
rm -rf "$PYTHON_DIR" "$VENV_DIR" "$BUNDLE_DIR"
mkdir -p "$BUILD_DIR"

# Download Python
echo ""
echo "=== Downloading Python ${PYTHON_VERSION} ==="
TEMP_FILE="/tmp/${FILENAME}"
if [[ ! -f "$TEMP_FILE" ]]; then
    curl -L -o "$TEMP_FILE" "$URL"
else
    echo "Using cached download: $TEMP_FILE"
fi

# Extract Python
echo ""
echo "=== Extracting Python ==="
mkdir -p "$PYTHON_DIR"
tar -xzf "$TEMP_FILE" -C "$PYTHON_DIR" --strip-components=1

# Verify Python works
echo ""
echo "=== Verifying Python ==="
"$PYTHON_DIR/$PYTHON_BIN" --version

# Create venv
echo ""
echo "=== Creating virtual environment ==="
"$PYTHON_DIR/$PYTHON_BIN" -m venv "$VENV_DIR"

# Install uv for fast package installation
echo ""
echo "=== Installing uv ==="
"$VENV_DIR/$VENV_PYTHON" -m pip install --upgrade pip uv

# Install ESPHome using uv
echo ""
echo "=== Installing ESPHome (using uv) ==="
"$VENV_DIR/$VENV_PYTHON" -m uv pip install esphome

# Verify ESPHome
echo ""
echo "=== Verifying ESPHome ==="
"$VENV_DIR/$VENV_PYTHON" -m esphome version

# Copy venv to bundle location and include Python lib
echo ""
echo "=== Preparing bundle ==="
cp -R "$VENV_DIR" "$BUNDLE_DIR"

# Copy the base Python lib directory (needed for libpython)
echo "Copying Python libraries..."
cp -R "$PYTHON_DIR/lib" "$BUNDLE_DIR/"

# Get size
BUNDLE_SIZE=$(du -sh "$BUNDLE_DIR" | cut -f1)
echo ""
echo "=== Bundle ready ==="
echo "Location: $BUNDLE_DIR"
echo "Size: $BUNDLE_SIZE"
echo ""
echo "You can now run 'cargo tauri build' to create the app bundle."
