#!/bin/sh
# Refuse to ship a binary that needs a newer glibc than the target has.
#
# This exists because we shipped one. `cargo build --release` on a Debian trixie
# box produced a dynamically linked hypha-sat requiring GLIBC_2.39; the first
# machine it landed on answered with "weak version `GLIBC_2.39' not found" and
# kept running anyway, which is worse than crashing — it looks like it worked.
# Everything under bin/ must be static musl, full stop.
set -eu
cd "$(dirname "$0")/.."

fail=0
for f in bin/*; do
    case "$f" in *.gitkeep) continue ;; esac
    [ -f "$f" ] || continue

    if ! file -b "$f" | grep -q 'statically linked\|static-pie'; then
        echo "FAIL $f: not statically linked ($(file -b "$f" | cut -d, -f1-2))" >&2
        fail=1
    fi

    vers=$(objdump -T "$f" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -u | tr '\n' ' ')
    if [ -n "$vers" ]; then
        echo "FAIL $f: requires glibc ($vers)" >&2
        fail=1
    fi

    # A binary that cannot even print its version on this box is not tested.
    case "$f" in
        *hypha*) "$f" --version >/dev/null 2>&1 || { echo "FAIL $f: --version failed" >&2; fail=1; } ;;
    esac
done

[ "$fail" = 0 ] && echo "binaries ok: static, no glibc, all run"
exit "$fail"
