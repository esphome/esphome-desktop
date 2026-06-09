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

# MinGit (minimal Git for Windows) is bundled on Windows only. ESPHome,
# PlatformIO and esphome-device-builder shell out to `git` for external
# components, github:// packages, voice models, ESP-IDF managed components and
# git+https:// deps; Windows ships no git, so without this the most common
# configs fail with a cryptic Python traceback (see issue #160). macOS prompts
# for the Command Line Tools and Linux ships git, so neither needs bundling.
# The full MinGit tree is required, not just git.exe: its bundled CA bundle
# (mingw64/etc/ssl/certs/ca-bundle.crt) and system gitconfig are what make
# HTTPS clones work without a system cert store or $HOME.
MINGIT_VERSION="2.54.0"
MINGIT_FILENAME="MinGit-${MINGIT_VERSION}-64-bit.zip"
MINGIT_URL="https://github.com/git-for-windows/git/releases/download/v${MINGIT_VERSION}.windows.1/${MINGIT_FILENAME}"
MINGIT_SHA256="04f937e1f0918b17b9be6f2294cb2bb66e96e1d9832d1c298e2de088a1d0e668"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/build"
BUNDLE_DIR="$PROJECT_DIR/src-tauri/python"
GIT_BUNDLE_DIR="$PROJECT_DIR/src-tauri/git"

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

# Resolve the published SHA-256 for a python-build-standalone asset from the
# release's SHA256SUMS manifest. Verifying against the same release we download
# from matches the trust model of pinning the URL and catches a corrupted,
# truncated, or cache-poisoned tarball. The manifest is cached under $BUILD_DIR.
pbs_expected_sha256() {
    local filename="$1"
    local sums_file="$BUILD_DIR/SHA256SUMS-${PBS_VERSION}"

    if [[ ! -f "$sums_file" ]]; then
        curl -fL --retry 3 -o "${sums_file}.partial" "${BASE_URL}/SHA256SUMS"
        mv -f "${sums_file}.partial" "$sums_file"
    fi

    local expected
    expected=$(awk -v f="$filename" '$2 == f {print $1}' "$sums_file")
    if [[ -z "$expected" ]]; then
        echo "No SHA-256 entry for $filename in SHA256SUMS" >&2
        exit 1
    fi
    echo "$expected"
}

# Download the python-build-standalone tarball for the given platform and
# extract it to $BUILD_DIR/python-${platform}. Downloads are cached under
# $BUILD_DIR (which we own) so re-runs don't re-download, and verified against
# the release's published SHA-256.
download_and_extract_python() {
    local platform="$1"
    local filename="$2"
    local python_dir="$BUILD_DIR/python-${platform}"
    local url="${BASE_URL}/${filename}"
    # Cache under $BUILD_DIR rather than the shared, world-writable /tmp so a
    # stale file owned by another user can't collide.
    local temp_file="$BUILD_DIR/${filename}"

    echo ""
    echo "=== Downloading Python ${PYTHON_VERSION} for ${platform} ==="
    local expected_sha
    expected_sha=$(pbs_expected_sha256 "$filename")
    if [[ -f "$temp_file" ]]; then
        echo "Using cached download: $temp_file"
        verify_sha256 "$temp_file" "$expected_sha"
    else
        # Atomic: download to .partial and promote to the cache path only once
        # the checksum passes, so an interrupted or corrupt download never
        # becomes a sticky, always-failing cache entry.
        curl -fL --retry 3 -o "${temp_file}.partial" "$url"
        verify_sha256 "${temp_file}.partial" "$expected_sha"
        mv -f "${temp_file}.partial" "$temp_file"
    fi

    echo ""
    echo "=== Extracting Python for ${platform} ==="
    rm -rf "$python_dir"
    mkdir -p "$python_dir"
    tar -xzf "$temp_file" -C "$python_dir" --strip-components=1
}

# Verify a file against an expected SHA-256, using whichever hashing tool the
# runner provides (sha256sum on Linux / git-bash, shasum on macOS). Exits on
# mismatch so a corrupted or tampered download can never make it into a bundle.
verify_sha256() {
    local file="$1"
    local expected="$2"
    local actual

    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$file" | cut -d' ' -f1)
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$file" | cut -d' ' -f1)
    else
        echo "No SHA-256 tool (sha256sum/shasum) available; cannot verify $file"
        exit 1
    fi

    if [[ "$actual" != "$expected" ]]; then
        echo "SHA-256 mismatch for $file"
        echo "  expected: $expected"
        echo "  actual:   $actual"
        exit 1
    fi
    echo "SHA-256 OK: $file"
}

# Stage git into $GIT_BUNDLE_DIR (src-tauri/git), bundled as a Tauri resource.
#
# Windows: download the pinned MinGit zip, verify its SHA-256, and extract the
# full tree (cmd/git.exe, mingw64/..., the CA bundle and system gitconfig). At
# runtime the app prepends git/cmd to PATH so ESPHome can find it.
#
# Other platforms: leave an empty directory so the "git" resource path in
# tauri.conf.json resolves; git is never bundled there (Linux ships it, macOS
# prompts for the Command Line Tools).
prepare_git_for_platform() {
    local platform="$1"

    rm -rf "$GIT_BUNDLE_DIR"
    mkdir -p "$GIT_BUNDLE_DIR"

    if [[ "$platform" != "windows-x64" ]]; then
        echo ""
        echo "=== Skipping git bundle (${platform}); empty git/ placeholder created ==="
        return
    fi

    # Cache under $BUILD_DIR (which we own and created) rather than the shared,
    # world-writable /tmp so a stale file owned by another user can't collide.
    local temp_file="$BUILD_DIR/${MINGIT_FILENAME}"

    echo ""
    echo "=== Downloading MinGit ${MINGIT_VERSION} ==="
    if [[ -f "$temp_file" ]]; then
        echo "Using cached download: $temp_file"
        verify_sha256 "$temp_file" "$MINGIT_SHA256"
    else
        # `--fail` so an HTTP error is a non-zero exit (not a 200-status error
        # body written to disk) and `--retry` to ride out transient blips.
        # Download to a .partial and only rename onto the cache path once the
        # checksum passes, so an interrupted or corrupt download never becomes a
        # sticky, always-failing cache entry (the fixed-path cache trap from
        # #152/#153).
        curl -fL --retry 3 -o "${temp_file}.partial" "$MINGIT_URL"
        verify_sha256 "${temp_file}.partial" "$MINGIT_SHA256"
        mv -f "${temp_file}.partial" "$temp_file"
    fi

    echo ""
    echo "=== Extracting MinGit into ${GIT_BUNDLE_DIR} ==="
    # Extract with the standalone Python we just unpacked rather than `unzip`,
    # which is not shipped with Git for Windows / git-bash and so is exactly the
    # tool most likely to be missing on the only runner this path runs on.
    # `python -m zipfile` is stdlib and guaranteed present here.
    local python_exe="$BUILD_DIR/python-${platform}/python.exe"
    if [[ ! -f "$python_exe" ]]; then
        echo "Bundled Python not found at $python_exe; cannot extract MinGit"
        exit 1
    fi
    "$python_exe" -m zipfile -e "$temp_file" "$GIT_BUNDLE_DIR"
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
prepare_git_for_platform "$PLATFORM"

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
if [[ "$PLATFORM" == "windows-x64" ]]; then
    GIT_BUNDLE_SIZE=$(du -sh "$GIT_BUNDLE_DIR" | cut -f1)
    echo "Bundled git: $GIT_BUNDLE_DIR ($GIT_BUNDLE_SIZE)"
fi
echo ""
echo "You can now run 'cargo tauri build' to create the app bundle."
