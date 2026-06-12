#!/usr/bin/env bash
# Builds the ntrack debug APK. Runs INSIDE the ntrack-builder container
# (see ../build.sh), with the repository mounted at /work.
#
# Environment:
#   ABIS        space-separated Android ABIs (default: "arm64-v8a";
#               also supported: armeabi-v7a x86_64)
#   SKIP_TESTS  set to 1 to skip the Rust test suite
#   CHOWN_UID/CHOWN_GID  hand artifact ownership to this host user

set -euo pipefail
cd /work

ABIS="${ABIS:-arm64-v8a}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/work/.docker-target}"
JNILIBS=android/app/src/main/jniLibs

if [ "${SKIP_TESTS:-0}" != "1" ]; then
    echo "==> Running Rust test suite"
    cargo test --workspace
fi

echo "==> Building Rust library for ABIs: $ABIS"
rm -rf "$JNILIBS"
TARGET_FLAGS=()
for abi in $ABIS; do TARGET_FLAGS+=(-t "$abi"); done
cargo ndk "${TARGET_FLAGS[@]}" --platform 26 -o "$JNILIBS" \
    build -p ntrack-app --lib --release --no-default-features --features android

# Bundle libc++_shared.so: Skia (Slint's Android renderer) links the shared
# C++ STL. cargo-ndk copies it for direct C++ deps, but be explicit.
for abi in $ABIS; do
    case "$abi" in
        arm64-v8a)   triple=aarch64-linux-android ;;
        armeabi-v7a) triple=arm-linux-androideabi ;;
        x86_64)      triple=x86_64-linux-android ;;
        *) echo "unsupported ABI: $abi" >&2; exit 1 ;;
    esac
    src="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/$triple/libc++_shared.so"
    if [ -f "$src" ] && [ ! -f "$JNILIBS/$abi/libc++_shared.so" ]; then
        cp "$src" "$JNILIBS/$abi/"
    fi
done

echo "==> Building debug APK"
ABIS_CSV="${ABIS// /,}"
(cd android && gradle --no-daemon -PntrackAbis="$ABIS_CSV" assembleDebug)

mkdir -p dist
cp android/app/build/outputs/apk/debug/app-debug.apk dist/ntrack-debug.apk

# Bind-mounted builds run as root; hand the artifacts back to the host user.
if [ -n "${CHOWN_UID:-}" ]; then
    chown -R "${CHOWN_UID}:${CHOWN_GID:-$CHOWN_UID}" \
        dist "$JNILIBS" android/app/build android/.gradle 2>/dev/null || true
fi

echo "==> Done: dist/ntrack-debug.apk"
ls -lh dist/ntrack-debug.apk
