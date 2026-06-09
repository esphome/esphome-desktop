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

PYTHON_VERSION="3.13.13"
PBS_VERSION="20260602"
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
            if [[ "$arch" == "aarch64" || "$arch" == "arm64" ]]; then
                echo "linux-arm64"
            else
                echo "linux-x64"
            fi
            ;;
        MINGW*|MSYS*|CYGWIN*)
            echo "windows-x64"
            ;;
        *)
            echo "unknown"
            ;;
    esac
}

# Compute the SHA-256 of a file using whichever tool the platform provides
# (Linux ships sha256sum, macOS ships shasum, and both are present under the
# git-bash environment on Windows runners). Prints the bare hex digest.
compute_sha256() {
    local out
    if command -v sha256sum >/dev/null 2>&1; then
        out=$(sha256sum "$1") || return 1
        awk '{print $1}' <<<"$out"
    elif command -v shasum >/dev/null 2>&1; then
        out=$(shasum -a 256 "$1") || return 1
        awk '{print $1}' <<<"$out"
    else
        echo "ERROR: no SHA-256 tool found (need sha256sum or shasum)" >&2
        return 1
    fi
}

# Download the python-build-standalone tarball for the given platform and
# extract it to $BUILD_DIR/python-${platform}. Tarball downloads are cached
# under $BUILD_DIR/cache (a path we own) so re-runs don't re-download.
#
# The interpreter is shipped verbatim to every user, so its integrity matters:
# we verify the download against the release's SHA256SUMS manifest. The
# previous version did no verification and reused any pre-existing /tmp file
# unconditionally — so an interrupted curl (partial tarball), an HTTP error
# page saved in place of the archive (curl lacked --fail), or a tampered cache
# would all be extracted and bundled silently. We now (1) fetch the expected
# digest, (2) re-verify cached files and re-download on mismatch, and (3)
# download to a temp path promoted to the cache only after the digest checks
# out, so a failed download never poisons the cache.
download_and_extract_python() {
    local platform="$1"
    local filename="$2"
    local python_dir="$BUILD_DIR/python-${platform}"
    local url="${BASE_URL}/${filename}"
    local cache_dir="$BUILD_DIR/cache"
    mkdir -p "$cache_dir"
    local temp_file="$cache_dir/${filename}"

    echo ""
    echo "=== Downloading Python ${PYTHON_VERSION} for ${platform} ==="

    local sums
    sums=$(curl -fsSL --retry 3 "${BASE_URL}/SHA256SUMS") || {
        echo "ERROR: failed to fetch ${BASE_URL}/SHA256SUMS" >&2
        exit 1
    }
    local expected_sha
    expected_sha=$(awk -v f="$filename" '$2 == f {print $1}' <<<"$sums")
    if [[ -z "$expected_sha" ]]; then
        echo "ERROR: no SHA256SUMS entry found for $filename" >&2
        exit 1
    fi

    if [[ -f "$temp_file" ]] && [[ "$(compute_sha256 "$temp_file")" == "$expected_sha" ]]; then
        echo "Using cached download: $temp_file"
    else
        [[ -f "$temp_file" ]] && echo "Cached file checksum mismatch — re-downloading"
        local partial="${temp_file}.partial.$$"
        if ! curl -fL --retry 3 -o "$partial" "$url"; then
            rm -f "$partial"
            echo "ERROR: failed to download $url" >&2
            exit 1
        fi
        local actual_sha
        actual_sha=$(compute_sha256 "$partial")
        if [[ "$actual_sha" != "$expected_sha" ]]; then
            rm -f "$partial"
            echo "ERROR: checksum mismatch for $filename" >&2
            echo "  expected: $expected_sha" >&2
            echo "  actual:   $actual_sha" >&2
            exit 1
        fi
        mv "$partial" "$temp_file"
        echo "Verified SHA-256: $actual_sha"
    fi

    echo ""
    echo "=== Extracting Python for ${platform} ==="
    rm -rf "$python_dir"
    mkdir -p "$python_dir"
    tar -xzf "$temp_file" -C "$python_dir" --strip-components=1
}

# Rewrite pip-generated script shebangs so the bundle is relocatable.
# pip bakes the build-time python path into every console-script shebang
# (`#!$python_dir/bin/python3`), so when the bundle ships to a user's
# machine the kernel can't find the interpreter and every `esphome`,
# `platformio`, `pip`, … invocation fails silently (see issue #34).
# Replace each shebang with the same sh/Python polyglot that
# python-build-standalone uses for its own scripts (idle3, pydoc3.13, …),
# so the whole bin/ directory is consistent and relocatable.
# Windows uses .exe launchers (not text scripts) so it's not called there.
make_scripts_relocatable() {
    local platform="$1"
    local python_dir="$2"

    echo ""
    echo "=== Making scripts relocatable (${platform}) ==="
    local py_major_minor="${PYTHON_VERSION%.*}"
    local rewritten=0
    local script first_line
    for script in "$python_dir/bin"/*; do
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
            "#!$python_dir/"*) ;;
            *) continue ;;
        esac
        {
            printf '%s\n' '#!/bin/sh'
            printf '%s\n' "'''exec' \"\$(dirname -- \"\$(realpath -- \"\$0\")\")/python${py_major_minor}\" \"\$0\" \"\$@\""
            printf '%s\n' "' '''"
            tail -n +2 "$script"
        } > "$script.relocatable"
        chmod +x "$script.relocatable"
        mv "$script.relocatable" "$script"
        rewritten=$((rewritten + 1))
    done
    echo "Rewrote shebangs in $rewritten scripts"
}

# Strip __pycache__ directories. Python regenerates .pyc files at runtime
# from the .py source, and the build-time .pyc files bake in absolute paths
# to the build directory (visible in tracebacks), so shipping them just
# bloats the bundle and leaks build paths to users.
strip_pycache() {
    local platform="$1"
    local python_dir="$2"

    echo ""
    echo "=== Stripping __pycache__ (${platform}) ==="
    local count
    count=$(find "$python_dir" -type d -name __pycache__ | wc -l)
    find "$python_dir" -type d -name __pycache__ -exec rm -rf {} +
    echo "Removed $count __pycache__ directories"
}

# Install ESPHome + ESPHome Device Builder into the standalone Python and
# post-process the install (rewrite shebangs, strip __pycache__) so the
# tree is relocatable and bundle-ready.
install_python_packages() {
    local platform="$1"
    local python_dir="$2"
    local python_bin="$3"

    echo ""
    echo "=== Verifying Python (${platform}) ==="
    "$python_dir/$python_bin" --version

    echo ""
    echo "=== Upgrading pip (${platform}) ==="
    "$python_dir/$python_bin" -m pip install --upgrade pip

    echo ""
    echo "=== Installing ESPHome (${platform}) ==="
    "$python_dir/$python_bin" -m pip install esphome

    echo ""
    echo "=== Verifying ESPHome (${platform}) ==="
    "$python_dir/$python_bin" -m esphome version

    # Install ESPHome Device Builder (the default backend). Pre-releases are
    # allowed so the bundle tracks the BuilderBeta channel that's wired up as
    # Backend::default() in src-tauri/src/settings/mod.rs.
    echo ""
    echo "=== Installing ESPHome Device Builder (${platform}) ==="
    "$python_dir/$python_bin" -m pip install --pre esphome-device-builder

    echo ""
    echo "=== Verifying ESPHome Device Builder (${platform}) ==="
    "$python_dir/$python_bin" -c "from importlib.metadata import version; print('esphome-device-builder', version('esphome-device-builder'))"

    if [[ "$platform" != "windows-x64" ]]; then
        make_scripts_relocatable "$platform" "$python_dir"
    fi

    strip_pycache "$platform" "$python_dir"
}

# Look up the PBS tarball + python binary for a platform, then download
# and install. Centralizes the platform table so adding a new platform is
# one entry here.
prepare_python_for_platform() {
    local platform="$1"
    local arch_os python_bin

    case "$platform" in
        macos-x64)    arch_os="x86_64-apple-darwin";       python_bin="bin/python3" ;;
        macos-arm64)  arch_os="aarch64-apple-darwin";      python_bin="bin/python3" ;;
        windows-x64)  arch_os="x86_64-pc-windows-msvc";    python_bin="python.exe"  ;;
        linux-x64)    arch_os="x86_64-unknown-linux-gnu";  python_bin="bin/python3" ;;
        linux-arm64)  arch_os="aarch64-unknown-linux-gnu"; python_bin="bin/python3" ;;
        *)
            echo "Unsupported platform: $platform"
            exit 1
            ;;
    esac

    local filename="cpython-${PYTHON_VERSION}+${PBS_VERSION}-${arch_os}-install_only_stripped.tar.gz"
    download_and_extract_python "$platform" "$filename"
    install_python_packages "$platform" "$BUILD_DIR/python-${platform}" "$python_bin"
}

PLATFORM="${1:-$(detect_platform)}"

if [[ "$PLATFORM" == "unknown" ]]; then
    echo "Could not detect platform. Please specify: macos-x64, macos-arm64, windows-x64, linux-x64, linux-arm64"
    exit 1
fi

echo "=== Preparing ESPHome bundle for ${PLATFORM} ==="

# Clean up previous builds
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUILD_DIR"

prepare_python_for_platform "$PLATFORM"

PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"

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
