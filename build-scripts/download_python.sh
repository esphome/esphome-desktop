#!/bin/bash
# Download python-build-standalone for bundling with ESPHome Desktop
#
# This script downloads relocatable Python builds from:
# https://github.com/indygreg/python-build-standalone
#
# Usage:
#   ./download_python.sh [platform]
#
# Platforms: macos-x64, macos-arm64, windows-x64, linux-x64, all
# Default: current platform

set -e

PYTHON_VERSION="3.13.0"
PBS_VERSION="20241016"  # python-build-standalone release date
BASE_URL="https://github.com/indygreg/python-build-standalone/releases/download/${PBS_VERSION}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
RESOURCES_DIR="$PROJECT_DIR/src-tauri/python"

download_python() {
    local platform=$1
    local filename
    local target_dir="$RESOURCES_DIR/$platform"

    case $platform in
        macos-x64)
            filename="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-apple-darwin-install_only.tar.gz"
            ;;
        macos-arm64)
            filename="cpython-${PYTHON_VERSION}+${PBS_VERSION}-aarch64-apple-darwin-install_only.tar.gz"
            ;;
        windows-x64)
            filename="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-pc-windows-msvc-shared-install_only.tar.gz"
            ;;
        linux-x64)
            filename="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-unknown-linux-gnu-install_only.tar.gz"
            ;;
        *)
            echo "Unknown platform: $platform"
            return 1
            ;;
    esac

    local url="${BASE_URL}/${filename}"
    local temp_file="/tmp/${filename}"

    echo "Downloading Python ${PYTHON_VERSION} for ${platform}..."
    echo "URL: ${url}"

    mkdir -p "$target_dir"

    # Download
    curl -L -o "$temp_file" "$url"

    # Extract
    echo "Extracting to ${target_dir}..."
    tar -xzf "$temp_file" -C "$target_dir" --strip-components=1

    # Clean up
    rm "$temp_file"

    echo "Done: ${target_dir}"
}

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

main() {
    local platform="${1:-$(detect_platform)}"

    if [[ "$platform" == "unknown" ]]; then
        echo "Could not detect platform. Please specify: macos-x64, macos-arm64, windows-x64, linux-x64"
        exit 1
    fi

    if [[ "$platform" == "all" ]]; then
        for p in macos-x64 macos-arm64 windows-x64 linux-x64; do
            download_python "$p"
        done
    else
        download_python "$platform"
    fi
}

main "$@"
