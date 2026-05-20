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

set -euo pipefail

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
    echo "Could not detect platform. Please specify: macos-x64, macos-arm64, macos-universal, windows-x64, linux-x64"
    exit 1
fi

is_macho() {
    local path="$1"
    file -b "$path" | grep -q "Mach-O"
}

# Files needed only at compile time (building C extensions, embedding Python).
# These are never identical between archs (arch-specific defines, paths,
# static libs) and aren't used at runtime, so strip them before the universal
# merge instead of trying to reconcile them.
strip_build_only_files() {
    local python_dir="$1"
    local py_major_minor="${PYTHON_VERSION%.*}"
    rm -rf "$python_dir/include"
    rm -rf "$python_dir/lib/python${py_major_minor}/config-${py_major_minor}-darwin"
    find "$python_dir/lib" -maxdepth 2 -type f \
        \( -name '*.a' \
        -o -name 'itclConfig.sh' \
        -o -name 'tclConfig.sh' \
        -o -name 'tkConfig.sh' \
        -o -name 'tdbcConfig.sh' \) -delete 2>/dev/null || true
}

# Combine two Mach-O files into a fat output. Handles the case where pip
# selected a universal2 wheel on one or both sides (e.g. orjson), so the
# file already covers both archs and lipo -create would fail on overlap.
merge_macho() {
    local a="$1" b="$2" out="$3"
    local a_archs b_archs
    a_archs=$(lipo -archs "$a" 2>/dev/null || true)
    b_archs=$(lipo -archs "$b" 2>/dev/null || true)
    if [[ "$a_archs" == *arm64* && "$a_archs" == *x86_64* ]]; then
        cp "$a" "$out"
    elif [[ "$b_archs" == *arm64* && "$b_archs" == *x86_64* ]]; then
        cp "$b" "$out"
    else
        lipo -create "$a" "$b" -output "$out"
    fi
}

# Metadata files whose arch-specific contents (Tag, RECORD hashes, sysconfig
# defines) don't affect runtime imports — Python imports the .so file by path,
# not via RECORD. Pick the arm64 copy arbitrarily.
prefer_arm_path() {
    case "$1" in
        *.dist-info/WHEEL|*.dist-info/RECORD|*.dist-info/REQUESTED) return 0;;
        *.dist-info/top_level.txt|*.dist-info/sboms/*) return 0;;
        */_sysconfigdata__darwin_darwin.py) return 0;;
    esac
    return 1
}

download_and_extract_python() {
    local platform="$1"
    local filename="$2"
    local python_dir="$BUILD_DIR/python-${platform}"
    local url="${BASE_URL}/${filename}"
    local temp_file="/tmp/${filename}"

    rm -rf "$python_dir"
    mkdir -p "$python_dir"

    echo ""
    echo "=== Downloading Python ${PYTHON_VERSION} for ${platform} ==="
    if [[ ! -f "$temp_file" ]]; then
        curl -L -o "$temp_file" "$url"
    else
        echo "Using cached download: $temp_file"
    fi

    echo ""
    echo "=== Extracting Python for ${platform} ==="
    tar -xzf "$temp_file" -C "$python_dir" --strip-components=1
}

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

    # Rewrite pip-generated script shebangs so the bundle is relocatable.
    # pip bakes the build-time python path into every console-script shebang
    # (`#!$python_dir/bin/python3`), so when the bundle ships to a user's
    # machine the kernel can't find the interpreter and every `esphome`,
    # `platformio`, `pip`, … invocation fails silently (see issue #34).
    # Replace each shebang with the same sh/Python polyglot that
    # python-build-standalone uses for its own scripts (idle3, pydoc3.13, …),
    # so the whole bin/ directory is consistent and relocatable.
    # Windows uses .exe launchers (not text scripts) so it's skipped here.
    if [[ "$platform" != "windows-x64" ]]; then
        echo ""
        echo "=== Making scripts relocatable (${platform}) ==="
        local py_major_minor="${PYTHON_VERSION%.*}"
        local rewritten=0
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
    fi

    # Strip __pycache__ directories. Python regenerates .pyc files at runtime
    # from the .py source, and the build-time .pyc files bake in absolute paths
    # to the build directory (visible in tracebacks), so shipping them just
    # bloats the bundle and leaks build paths to users.
    echo ""
    echo "=== Stripping __pycache__ (${platform}) ==="
    local pycache_count
    pycache_count=$(find "$python_dir" -type d -name __pycache__ | wc -l)
    find "$python_dir" -type d -name __pycache__ -exec rm -rf {} +
    echo "Removed $pycache_count __pycache__ directories"
}

merge_universal_python() {
    local arm_dir="$1"
    local x64_dir="$2"
    local universal_dir="$3"

    if ! command -v lipo >/dev/null 2>&1; then
        echo "Error: lipo is required to build macos-universal bundles"
        exit 1
    fi

    strip_build_only_files "$arm_dir"
    strip_build_only_files "$x64_dir"

    rm -rf "$universal_dir"
    cp -a "$arm_dir" "$universal_dir"

    local dropped_archspec=0

    while IFS= read -r -d '' arm_path; do
        local rel_path="${arm_path#$arm_dir/}"
        local x64_path="$x64_dir/$rel_path"
        local universal_path="$universal_dir/$rel_path"

        # Directories are created by cp -a; nothing to merge.
        [[ -d "$arm_path" && ! -L "$arm_path" ]] && continue

        # Symlinks: cp -a preserved arm64's; if x64 has the same link, no work;
        # if only arm64 has it, the arm copy is the right answer.
        [[ -L "$arm_path" ]] && continue

        # Only present on arm64. Drop arch-specific .so/.dylib so x86_64 doesn't
        # try to dlopen a wrong-arch binary at import time (pure-Python fallback
        # in the package will kick in on both archs). Keep everything else.
        if [[ ! -e "$x64_path" ]]; then
            if [[ "$arm_path" == *.so || "$arm_path" == *.dylib ]] && is_macho "$arm_path"; then
                local archs
                archs=$(lipo -archs "$arm_path" 2>/dev/null || true)
                if [[ "$archs" != *x86_64* ]]; then
                    rm -f "$universal_path"
                    dropped_archspec=$((dropped_archspec + 1))
                fi
            fi
            continue
        fi

        # Identical: cp -a already placed the arm copy.
        cmp -s "$arm_path" "$x64_path" && continue

        if is_macho "$arm_path" && is_macho "$x64_path"; then
            merge_macho "$arm_path" "$x64_path" "$universal_path"
            [[ -x "$arm_path" ]] && chmod +x "$universal_path"
            continue
        fi

        if prefer_arm_path "$rel_path"; then
            continue
        fi

        echo "Error: non-Mach-O file differs between arm64 and x64 bundles: $rel_path"
        exit 1
    done < <(find "$arm_dir" -mindepth 1 -print0)

    # Bring across files only present on x64 (transitive deps that resolved
    # differently between runs). Drop arch-specific .so/.dylib for the same
    # reason as above.
    while IFS= read -r -d '' x64_path; do
        local rel_path="${x64_path#$x64_dir/}"
        [[ -e "$arm_dir/$rel_path" ]] && continue
        if [[ "$x64_path" == *.so || "$x64_path" == *.dylib ]] && is_macho "$x64_path"; then
            local archs
            archs=$(lipo -archs "$x64_path" 2>/dev/null || true)
            if [[ "$archs" != *arm64* ]]; then
                dropped_archspec=$((dropped_archspec + 1))
                continue
            fi
        fi
        mkdir -p "$(dirname "$universal_dir/$rel_path")"
        cp -a "$x64_path" "$universal_dir/$rel_path"
    done < <(find "$x64_dir" -mindepth 1 -print0)

    if (( dropped_archspec > 0 )); then
        echo "Dropped $dropped_archspec arch-specific binaries (packages will use pure-Python fallback)"
    fi

    local merged_archs
    merged_archs=$(lipo -archs "$universal_dir/bin/python3.13" 2>/dev/null || true)
    if [[ "$merged_archs" != *arm64* || "$merged_archs" != *x86_64* ]]; then
        echo "Error: merged python3.13 is not universal (archs: $merged_archs)"
        exit 1
    fi
}

echo "=== Preparing ESPHome bundle for ${PLATFORM} ==="

# Clean up previous builds
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUILD_DIR"

case "$PLATFORM" in
    macos-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-apple-darwin-install_only_stripped.tar.gz"
        PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"
        download_and_extract_python "$PLATFORM" "$FILENAME"
        install_python_packages "$PLATFORM" "$PYTHON_DIR" "bin/python3"
        ;;
    macos-arm64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-aarch64-apple-darwin-install_only_stripped.tar.gz"
        PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"
        download_and_extract_python "$PLATFORM" "$FILENAME"
        install_python_packages "$PLATFORM" "$PYTHON_DIR" "bin/python3"
        ;;
    windows-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-pc-windows-msvc-install_only_stripped.tar.gz"
        PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"
        download_and_extract_python "$PLATFORM" "$FILENAME"
        install_python_packages "$PLATFORM" "$PYTHON_DIR" "python.exe"
        ;;
    linux-x64)
        FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz"
        PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"
        download_and_extract_python "$PLATFORM" "$FILENAME"
        install_python_packages "$PLATFORM" "$PYTHON_DIR" "bin/python3"
        ;;
    macos-universal)
        ARM_FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-aarch64-apple-darwin-install_only_stripped.tar.gz"
        X64_FILENAME="cpython-${PYTHON_VERSION}+${PBS_VERSION}-x86_64-apple-darwin-install_only_stripped.tar.gz"
        ARM_DIR="$BUILD_DIR/python-macos-arm64"
        X64_DIR="$BUILD_DIR/python-macos-x64"
        PYTHON_DIR="$BUILD_DIR/python-${PLATFORM}"

        download_and_extract_python "macos-arm64" "$ARM_FILENAME"
        install_python_packages "macos-arm64" "$ARM_DIR" "bin/python3"

        download_and_extract_python "macos-x64" "$X64_FILENAME"
        install_python_packages "macos-x64" "$X64_DIR" "bin/python3"

        echo ""
        echo "=== Merging macOS universal Python bundle ==="
        merge_universal_python "$ARM_DIR" "$X64_DIR" "$PYTHON_DIR"
        ;;
    *)
        echo "Unsupported platform: $PLATFORM"
        exit 1
        ;;
esac

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
