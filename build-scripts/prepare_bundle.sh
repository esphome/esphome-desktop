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

PYTHON_VERSION="3.13.14"
PBS_VERSION="20260623"
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
# MINGIT_VERSION is display-only; MINGIT_URL is the literal asset URL rather
# than something rebuilt from the version, because Git for Windows rebuilds
# encode their build number differently in the tag and the filename (e.g. tag
# v2.53.0.windows.3 ships MinGit-2.53.0.3-64-bit.zip). All three are rewritten
# by the nightly `bump_bundle_versions.py --target mingit` job.
MINGIT_VERSION="2.55.0.2"
MINGIT_URL="https://github.com/git-for-windows/git/releases/download/v2.55.0.windows.2/MinGit-2.55.0.2-64-bit.zip"
MINGIT_SHA256="e3ea2944cea4b3fabcd69c7c1669ef69b1b66c05ac7806d81224d0abad2dec31"

# PortableGit is downloaded (Windows only) solely to harvest a GNU `patch.exe`:
# MinGit doesn't ship one, but the esphome micro-opus component's ESP-IDF build
# needs `patch` on PATH to patch the Opus source (issue #189). We extract only
# patch.exe plus the MSYS runtime DLLs it links into a dedicated `git/patch/`
# dir rather than bundling the ~300MB PortableGit tree. Pinned to the same
# git-for-windows release as MinGit so patch.exe and msys-2.0.dll match, and
# rewritten alongside MINGIT_* by the nightly `bump_bundle_versions.py` job.
PORTABLEGIT_URL="https://github.com/git-for-windows/git/releases/download/v2.55.0.windows.2/PortableGit-2.55.0.2-64-bit.7z.exe"
PORTABLEGIT_SHA256="b20d42da3afa228e9fa6174480de820282667e799440d655e308f700dfa0d0df"

# ccache is bundled on Windows only. ESPHome's ESP-IDF builds auto-enable ccache
# when the `ccache` binary is on PATH, which roughly halves repeat-build times;
# Windows ships no ccache and users rarely install one, so we bundle the official
# static Windows build and put it on PATH at runtime. macOS finds a brew-installed
# ccache via the appended Homebrew PATH, and Linux users install it from their
# distro, so neither bundles it. The Windows release zip nests a single static
# `ccache.exe` (no DLLs) under a versioned dir; we extract just that exe.
# CCACHE_VERSION is display-only; CCACHE_URL is the literal asset URL. There is
# no upstream checksum file (only minisig), so CCACHE_SHA256 is computed from the
# pinned asset. All three are rewritten by the nightly
# `bump_bundle_versions.py --target ccache` job.
CCACHE_VERSION="4.13.6"
CCACHE_URL="https://github.com/ccache/ccache/releases/download/v4.13.6/ccache-4.13.6-windows-x86_64.zip"
CCACHE_SHA256="3d7cebb05850ad704e197b3f1d3f0f924ab6c9fdfc561578e146184fe9d89380"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/build"
BUNDLE_DIR="$PROJECT_DIR/src-tauri/python"
GIT_BUNDLE_DIR="$PROJECT_DIR/src-tauri/git"
CCACHE_BUNDLE_DIR="$PROJECT_DIR/src-tauri/ccache"

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

# Verify a file against an expected SHA-256, exiting on mismatch. Thin wrapper
# over compute_sha256 used by the MinGit download, whose expected digest is a
# pinned constant rather than a SHA256SUMS lookup.
verify_sha256() {
    local file="$1"
    local expected="$2"
    local actual
    actual=$(compute_sha256 "$file") || exit 1
    if [[ "$actual" != "$expected" ]]; then
        echo "ERROR: checksum mismatch for $file" >&2
        echo "  expected: $expected" >&2
        echo "  actual:   $actual" >&2
        exit 1
    fi
    echo "Verified SHA-256: $actual"
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
    local temp_file="$BUILD_DIR/$(basename "$MINGIT_URL")"

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

    prepare_patch_for_windows
}

# Harvest a GNU `patch.exe` from PortableGit into a dedicated `git/patch/` dir
# (issue #189). MinGit ships no patch, but the esphome micro-opus ESP-IDF build
# needs `patch` on PATH. We extract only patch.exe and the MSYS DLLs it links —
# co-located so Windows resolves them from the exe's own directory — instead of
# shipping the ~300MB PortableGit tree or exposing all of MinGit's usr/bin
# (whose sh/find/sort would shadow Windows built-ins in the build).
prepare_patch_for_windows() {
    local patch_dir="$GIT_BUNDLE_DIR/patch"
    local temp_file="$BUILD_DIR/$(basename "$PORTABLEGIT_URL")"

    # PortableGit is a 7-Zip self-extracting archive. 7z is preinstalled on the
    # windows-latest CI runner; for a local Windows build, install 7-Zip and put
    # `7z` on PATH. Fail with a clear message rather than a bare "command not
    # found" from the extract step below.
    if ! command -v 7z >/dev/null 2>&1; then
        echo "ERROR: 7z (7-Zip) is required to extract patch.exe from PortableGit." >&2
        echo "       Install 7-Zip and ensure '7z' is on PATH." >&2
        exit 1
    fi

    echo ""
    echo "=== Downloading PortableGit (for patch.exe) ==="
    if [[ -f "$temp_file" ]]; then
        echo "Using cached download: $temp_file"
        verify_sha256 "$temp_file" "$PORTABLEGIT_SHA256"
    else
        curl -fL --retry 3 -o "${temp_file}.partial" "$PORTABLEGIT_URL"
        verify_sha256 "${temp_file}.partial" "$PORTABLEGIT_SHA256"
        mv -f "${temp_file}.partial" "$temp_file"
    fi

    echo ""
    echo "=== Extracting patch.exe into ${patch_dir} ==="
    # PortableGit is a 7-Zip self-extracting archive; `7z` is preinstalled on the
    # windows-latest runner this path runs on. Pull only patch.exe and the MSYS
    # runtime DLLs it depends on (msys-2.0.dll + gettext/iconv), flattened into
    # patch_dir so they sit beside the exe.
    mkdir -p "$patch_dir"
    7z e "$temp_file" -o"$patch_dir" -y \
        usr/bin/patch.exe \
        usr/bin/msys-2.0.dll \
        usr/bin/msys-intl-8.dll \
        usr/bin/msys-iconv-2.dll

    if [[ ! -f "$patch_dir/patch.exe" ]]; then
        echo "ERROR: patch.exe not extracted from PortableGit" >&2
        exit 1
    fi

    # We ship only the binaries (not PortableGit's license tree), so accompany
    # them with the required attribution + corresponding-source pointer for these
    # GPL/LGPL components.
    cat > "$patch_dir/THIRD_PARTY_NOTICES.txt" <<EOF
This directory contains unmodified binaries taken from Git for Windows
(${PORTABLEGIT_URL}):

  patch.exe         GNU patch          GPLv3
  msys-2.0.dll      MSYS2 runtime      LGPLv3
  msys-intl-8.dll   GNU gettext libintl  LGPLv2.1-or-later
  msys-iconv-2.dll  GNU libiconv         LGPLv2.1-or-later

They are redistributed under their respective licenses and are invoked as a
separate process (GNU patch) by ESPHome's build. Corresponding source is
available from the Git for Windows release above and its upstream projects.
EOF

    # Smoke-test: prove patch runs (and that no DLL dependency is missing). If a
    # future MSYS patch needs more than the DLLs above, this fails the build here
    # rather than shipping a patch.exe that won't launch on a user's machine.
    echo "=== Verifying bundled patch.exe ==="
    "$patch_dir/patch.exe" --version
}

# Stage ccache into $CCACHE_BUNDLE_DIR (src-tauri/ccache), bundled as a Tauri
# resource. At runtime the app prepends this dir to PATH so ESPHome's ESP-IDF
# builds discover ccache and turn caching on automatically.
#
# Windows: download the pinned ccache zip, verify its SHA-256, and extract just
# the single static ccache.exe (the zip also carries docs/license we don't ship).
#
# Other platforms: leave an empty directory so the "ccache" resource path in
# tauri.conf.json resolves; ccache is never bundled there (macOS finds a brew
# ccache via the appended Homebrew PATH, Linux installs it from the distro).
prepare_ccache_for_platform() {
    local platform="$1"

    rm -rf "$CCACHE_BUNDLE_DIR"
    mkdir -p "$CCACHE_BUNDLE_DIR"

    if [[ "$platform" != "windows-x64" ]]; then
        echo ""
        echo "=== Skipping ccache bundle (${platform}); empty ccache/ placeholder created ==="
        return
    fi

    # Cache under $BUILD_DIR (which we own) rather than world-writable /tmp, same
    # as the MinGit download.
    local temp_file="$BUILD_DIR/$(basename "$CCACHE_URL")"

    echo ""
    echo "=== Downloading ccache ${CCACHE_VERSION} ==="
    if [[ -f "$temp_file" ]]; then
        echo "Using cached download: $temp_file"
        verify_sha256 "$temp_file" "$CCACHE_SHA256"
    else
        # `--fail`/`--retry` and the .partial->rename dance mirror the MinGit
        # download so an interrupted or corrupt fetch never becomes a sticky,
        # always-failing cache entry.
        curl -fL --retry 3 -o "${temp_file}.partial" "$CCACHE_URL"
        verify_sha256 "${temp_file}.partial" "$CCACHE_SHA256"
        mv -f "${temp_file}.partial" "$temp_file"
    fi

    echo ""
    echo "=== Extracting ccache.exe into ${CCACHE_BUNDLE_DIR} ==="
    # Extract with the standalone Python we just unpacked rather than `unzip`,
    # which isn't shipped on the Windows runner (same reasoning as MinGit). The
    # zip nests ccache.exe (plus docs and the license) under a versioned dir, so
    # extract to a temp dir then copy just the exe and the license into the flat
    # bundle dir.
    local python_exe="$BUILD_DIR/python-${platform}/python.exe"
    if [[ ! -f "$python_exe" ]]; then
        echo "Bundled Python not found at $python_exe; cannot extract ccache"
        exit 1
    fi
    local extract_dir="$BUILD_DIR/ccache-extract"
    rm -rf "$extract_dir"
    "$python_exe" -m zipfile -e "$temp_file" "$extract_dir"

    # Require exactly one ccache.exe so a future zip layout change (extra arch,
    # helper exe) fails loudly here rather than silently bundling whichever copy
    # `find` happened to list first.
    local -a exe_matches=()
    while IFS= read -r match; do
        exe_matches+=("$match")
    done < <(find "$extract_dir" -type f -name ccache.exe)
    if [[ ${#exe_matches[@]} -ne 1 ]]; then
        echo "ERROR: expected exactly one ccache.exe in $CCACHE_URL, found ${#exe_matches[@]}" >&2
        exit 1
    fi
    local ccache_src_dir
    ccache_src_dir=$(dirname "${exe_matches[0]}")
    cp "${exe_matches[0]}" "$CCACHE_BUNDLE_DIR/ccache.exe"

    # ccache is GPLv3, so ship its full license text (carried in the release zip)
    # next to the binary, not just a pointer, to satisfy the redistribution
    # terms. Fail if it's missing rather than ship an unlicensed binary.
    if [[ ! -f "$ccache_src_dir/LICENSE.md" ]]; then
        echo "ERROR: LICENSE.md not found beside ccache.exe in $CCACHE_URL" >&2
        exit 1
    fi
    cp "$ccache_src_dir/LICENSE.md" "$CCACHE_BUNDLE_DIR/LICENSE.md"

    cat > "$CCACHE_BUNDLE_DIR/THIRD_PARTY_NOTICES.txt" <<EOF
This directory contains an unmodified ccache.exe taken from the official ccache
release (${CCACHE_URL}):

  ccache.exe        ccache             GPLv3

It is redistributed under its license (see the bundled LICENSE.md) and is
invoked as a separate process (compiler cache) by ESPHome's build. Corresponding
source is available from the ccache release above and
https://github.com/ccache/ccache.
EOF

    # Smoke-test: prove ccache runs, so a broken extract fails the build here
    # rather than shipping a ccache.exe that won't launch on a user's machine.
    echo "=== Verifying bundled ccache.exe ==="
    "$CCACHE_BUNDLE_DIR/ccache.exe" --version
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
prepare_ccache_for_platform "$PLATFORM"

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
    CCACHE_BUNDLE_SIZE=$(du -sh "$CCACHE_BUNDLE_DIR" | cut -f1)
    echo "Bundled ccache: $CCACHE_BUNDLE_DIR ($CCACHE_BUNDLE_SIZE)"
fi
echo ""
echo "You can now run 'cargo tauri build' to create the app bundle."
