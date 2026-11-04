#!/bin/sh
# Build static musl binaries and stage them where the hub serves /dl from.
#   scripts/build.sh            # host arch only
#   scripts/build.sh all        # x86_64 + aarch64
set -eu
cd "$(dirname "$0")/.."

stage() {
    target="$1"; plat="$2"; arch="$3"
    echo "==> $target"
    if command -v cross >/dev/null 2>&1; then
        cross build --release --target "$target"
    else
        rustup target add "$target" >/dev/null 2>&1 || true
        cargo build --release --target "$target"
    fi
    out="target/$target/release"
    cp "$out/hypha-sat" "bin/hypha-sat-$plat-$arch"
    cp "$out/hyphad"    "bin/hyphad-$plat-$arch"
    cp "$out/hypha"     "bin/hypha-$plat-$arch"
}

case "${1:-host}" in
    all)
        stage x86_64-unknown-linux-musl linux x86_64
        stage aarch64-unknown-linux-musl linux aarch64
        ;;
    *)
        stage "$(rustc -vV | awk '/host:/{print $2}')" linux "$(rustc -vV | awk '/host:/{print $2}' | cut -d- -f1)"
        ;;
esac

# Also stage plain names so `install.sh` works on the build host.
cp "bin/hypha-sat-linux-$(uname -m)" bin/hypha-sat 2>/dev/null || true
ls -lh bin/
