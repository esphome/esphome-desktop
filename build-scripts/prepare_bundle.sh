#!/bin/bash
# Prepare the Python environment with ESPHome installed for bundling
#
# This script:
# 1. Downloads python-build-standalone
# 2. Installs ESPHome and ESPHome Device Builder directly into the
#    standalone Python (no venv)
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
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-apple-darwin-install_only_stripped.tar.gz"
        PYTHON_BIN="bin/python3"
        ;;
    macos-arm64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-aarch64-apple-darwin-install_only_stripped.tar.gz"
        PYTHON_BIN="bin/python3"
        ;;
    windows-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"
        PYTHON_BIN="python.exe"
        ;;
    linux-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz"
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

# Upgrade pip
echo ""
echo "=== Upgrading pip ==="
"$PYTHON_DIR/$PYTHON_BIN" -m pip install --upgrade pip

# Install ESPHome directly into standalone Python (no venv)
echo ""
echo "=== Installing ESPHome ==="
"$PYTHON_DIR/$PYTHON_BIN" -m pip install esphome

# Verify ESPHome
echo ""
echo "=== Verifying ESPHome ==="
"$PYTHON_DIR/$PYTHON_BIN" -m esphome version

# Install ESPHome Device Builder (the default backend). Pre-releases are
# allowed so the bundle tracks the BuilderBeta channel that's wired up as
# Backend::default() in src-tauri/src/settings/mod.rs.
echo ""
echo "=== Installing ESPHome Device Builder ==="
"$PYTHON_DIR/$PYTHON_BIN" -m pip install --pre esphome-device-builder

# Verify ESPHome Device Builder
echo ""
echo "=== Verifying ESPHome Device Builder ==="
"$PYTHON_DIR/$PYTHON_BIN" -c "from importlib.metadata import version; print('esphome-device-builder', version('esphome-device-builder'))"

# Rewrite pip-generated script shebangs so the bundle is relocatable.
# pip bakes the build-time python path into every console-script shebang
# (`#!$PYTHON_DIR/bin/python3`), so when the bundle ships to a user's
# machine the kernel can't find the interpreter and every `esphome`,
# `platformio`, `pip`, … invocation fails silently (see issue #34).
# Replace each shebang with the same sh/Python polyglot that
# python-build-standalone uses for its own scripts (idle3, pydoc3.13, …),
# so the whole bin/ directory is consistent and relocatable.
# Windows uses .exe launchers (not text scripts) so it's skipped here.
if [[ "$PLATFORM" != "windows-x64" ]]; then
    echo ""
    echo "=== Making scripts relocatable ==="
    PY_MAJOR_MINOR="${PYTHON_VERSION%.*}"
    REWRITTEN=0
    for script in "$PYTHON_DIR/bin"/*; do
        [[ -f "$script" ]] || continue
        # Skip symlinks (their targets are processed as regular files in this
        # same loop, and the symlinks pick up the rewritten content).
        [[ -L "$script" ]] && continue
        # Skip the python3 executable itself and any other Mach-O / ELF binary.
        if file -b "$script" | grep -qE 'Mach-O|ELF'; then
            continue
        fi
        # Only rewrite scripts whose shebang points into the build python.
        first_line=$(head -n1 "$script" 2>/dev/null) || continue
        case "$first_line" in
            "#!$PYTHON_DIR/"*) ;;
            *) continue ;;
        esac
        {
            printf '%s\n' '#!/bin/sh'
            printf '%s\n' "'''exec' \"\$(dirname -- \"\$(realpath -- \"\$0\")\")/python${PY_MAJOR_MINOR}\" \"\$0\" \"\$@\""
            printf '%s\n' "' '''"
            tail -n +2 "$script"
        } > "$script.relocatable"
        chmod +x "$script.relocatable"
        mv "$script.relocatable" "$script"
        REWRITTEN=$((REWRITTEN + 1))
    done
    echo "Rewrote shebangs in $REWRITTEN scripts"
fi

# Strip __pycache__ directories. Python regenerates .pyc files at runtime
# from the .py source, and the build-time .pyc files bake in absolute paths
# to the build directory (visible in tracebacks), so shipping them just
# bloats the bundle and leaks build paths to users.
echo ""
echo "=== Stripping __pycache__ ==="
PYCACHE_COUNT=$(find "$PYTHON_DIR" -type d -name __pycache__ | wc -l)
find "$PYTHON_DIR" -type d -name __pycache__ -exec rm -rf {} +
echo "Removed $PYCACHE_COUNT __pycache__ directories"

# Copy Python directory to bundle location. Wipe any prior bundle first so
# `cp -R` can't fall back to merge behavior on top of a partial previous run.
echo ""
echo "=== Preparing bundle ==="
rm -rf "$BUNDLE_DIR"
cp -R "$PYTHON_DIR" "$BUNDLE_DIR"

# Get size
BUNDLE_SIZE=$(du -sh "$BUNDLE_DIR" | cut -f1)
echo ""
echo "=== Bundle ready ==="
echo "Location: $BUNDLE_DIR"
echo "Size: $BUNDLE_SIZE"
echo ""
echo "You can now run 'cargo tauri build' to create the app bundle."
