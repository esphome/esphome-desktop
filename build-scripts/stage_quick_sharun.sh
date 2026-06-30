#!/bin/bash
# Stage a patched quick-sharun.sh for the Tauri AppImage build.
#
# The Tauri AppImage fork downloads quick-sharun.sh from upstream main and runs
# it during `cargo tauri build`. Its strace step (which discovers dlopen'd
# libraries) backgrounds each traced binary and stops it with `set -m` job
# control plus a process group kill. On tty-less CI runners `set -m` silently
# no-ops, so the kill never reaches webkit2gtk's MiniBrowser and the build hangs
# until the job timeout (pkgforge-dev/Anylinux-AppImages regression, late June).
#
# Pre-stage a copy pinned to a known upstream commit, patched to bound each
# trace with `timeout` instead. The bundler only downloads quick-sharun.sh when
# the file is absent, so staging ours here makes it reuse our copy. strace lib
# detection is preserved.

set -euo pipefail

SHARUN_REV="867c0b1543321149a133565c41cf19d81dc74533"
SHARUN_SHA256="bcf0496dc2a374734750bcc6f74ce14793cb8539cbeefdecf40bd818fad36a82"

CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/tauri"
DEST="$CACHE_DIR/quick-sharun.sh"
mkdir -p "$CACHE_DIR"

curl -fL --retry 3 --retry-delay 2 -o "$DEST" \
  "https://raw.githubusercontent.com/pkgforge-dev/Anylinux-AppImages/${SHARUN_REV}/useful-tools/quick-sharun.sh"
echo "${SHARUN_SHA256}  ${DEST}" | sha256sum -c -

# Replace the strace kill block. A pinned SHA256 plus the exact-match assert
# below fail loudly if the upstream file drifts, prompting a re-pin rather than
# a silent regression.
python3 - "$DEST" <<'PY'
import sys

p = sys.argv[1]
src = open(p).read()
old = (
    '\t\t_echo "STRACE: [$b] ..."\n'
    '\t\tset -m\n'
    '\t\tif [ -n "$XVFB_CMD" ]; then\n'
    '\t\t\t$XVFB_CMD env LD_DEBUG=libs "$b" $flags >/dev/null 2>"$dlopened" &\n'
    '\t\telse\n'
    '\t\t\tLD_DEBUG=libs "$b" $flags >/dev/null 2>"$dlopened" &\n'
    '\t\tfi\n'
    '\t\tpid=$!\n'
    '\t\tset +m\n'
    '\n'
    '\t\tsleep "$STRACE_TIME"\n'
    '\t\tkill -TERM -$pid 2>/dev/null || :\n'
    '\t\tsleep 1\n'
    '\t\tkill -KILL -$pid 2>/dev/null || :\n'
    '\t\twait $pid 2>/dev/null || :\n'
)
new = (
    '\t\t_echo "STRACE: [$b] ..."\n'
    '\t\t# set -m no-ops on tty-less CI; bound each trace with timeout.\n'
    '\t\tif [ -n "$XVFB_CMD" ]; then\n'
    '\t\t\ttimeout -k 1 "$STRACE_TIME" $XVFB_CMD env LD_DEBUG=libs "$b" $flags >/dev/null 2>"$dlopened" || :\n'
    '\t\telse\n'
    '\t\t\ttimeout -k 1 "$STRACE_TIME" env LD_DEBUG=libs "$b" $flags >/dev/null 2>"$dlopened" || :\n'
    '\t\tfi\n'
)
if src.count(old) != 1:
    sys.exit("quick-sharun.sh strace block not found exactly once; "
             "upstream shape changed, re-pin SHARUN_REV/SHA256")
open(p, "w").write(src.replace(old, new))
PY

chmod +x "$DEST"
echo "Staged patched quick-sharun at $DEST"
