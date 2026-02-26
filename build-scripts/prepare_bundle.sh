#!/bin/bash
# Prepare the Python environment with ESPHome pre-installed for bundling
#
# This script:
# 1. Downloads python-build-standalone
# 2. Installs ESPHome directly into the standalone Python (no venv)
# 3. Prepares it for bundling with the app
#
# Note: No venv is used to avoid absolute path issues in bundled executables
#
# Usage: ./prepare_bundle.sh [platform]

set -e

PYTHON_VERSION="3.13.12"
PBS_VERSION="20260203"
BASE_URL="https://github.com/astral-sh/python-build-standalone/releases/download/${PBS_VERSION}"

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
        ;;
    macos-arm64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-aarch64-apple-darwin-install_only.tar.gz"
        PYTHON_BIN="bin/python3"
        ;;
    windows-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-pc-windows-msvc-install_only.tar.gz"
        PYTHON_BIN="python.exe"
        ;;
    linux-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-unknown-linux-gnu-install_only.tar.gz"
        PYTHON_BIN="bin/python3"
        ;;
esac

URL="${BASE_URL}/${FILENAME}"
PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"

echo "=== Preparing ESPHome bundle for ${PLATFORM} ==="

# Clean up previous builds
rm -rf "$PYTHON_DIR" "$BUNDLE_DIR"
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

# Install uv for fast package installation
echo ""
echo "=== Installing uv ==="
"$PYTHON_DIR/$PYTHON_BIN" -m pip install --upgrade pip uv

# Install ESPHome directly into standalone Python (no venv)
echo ""
echo "=== Installing ESPHome (using uv) ==="
"$PYTHON_DIR/$PYTHON_BIN" -m uv pip install esphome

# Verify ESPHome
echo ""
echo "=== Verifying ESPHome ==="
"$PYTHON_DIR/$PYTHON_BIN" -m esphome version

# Copy Python directory to bundle location
echo ""
echo "=== Preparing bundle ==="
cp -R "$PYTHON_DIR" "$BUNDLE_DIR"

# Create portable wrapper scripts for esphome
echo ""
echo "=== Creating portable esphome wrappers ==="

case $PLATFORM in
    windows-x64)
        # Create esphome.bat wrappers for compatibility
        mkdir -p "$BUNDLE_DIR/Scripts"

        cat > "$BUNDLE_DIR/Scripts/esphome.bat" << 'EOF'
@echo off
"%~dp0..\python.exe" -m esphome %*
EOF

        cat > "$BUNDLE_DIR/esphome.bat" << 'EOF'
@echo off
"%~dp0python.exe" -m esphome %*
EOF

        echo "Created esphome.bat wrappers for Windows"
        ;;
    *)
        # Remove any broken pip-generated scripts
        rm -f "$BUNDLE_DIR/bin/esphome" 2>/dev/null || true

        # Create esphome wrapper for Unix (macOS/Linux) in bin directory
        cat > "$BUNDLE_DIR/bin/esphome" << 'EOF'
#!/bin/sh
DIR="$(cd "$(dirname "$0")" && pwd)"
exec "$DIR/python3" -m esphome "$@"
EOF
        chmod +x "$BUNDLE_DIR/bin/esphome"
        echo "Created esphome shell wrapper"
        ;;
esac

# Get size
BUNDLE_SIZE=$(du -sh "$BUNDLE_DIR" | cut -f1)
echo ""
echo "=== Bundle ready ==="
echo "Location: $BUNDLE_DIR"
echo "Size: $BUNDLE_SIZE"
echo ""
echo "You can now run 'cargo tauri build' to create the app bundle."
