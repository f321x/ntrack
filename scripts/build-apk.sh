#!/usr/bin/env bash
# Builds the ntrack APK (debug or release). Runs INSIDE the ntrack-builder
# container (see ../build.sh), with the repository mounted at /work.
#
# Environment:
#   ABIS        space-separated Android ABIs (default: "arm64-v8a";
#               also supported: armeabi-v7a x86_64)
#   SKIP_TESTS  set to 1 to skip the Rust test suite
#   BUILD_TYPE  "debug" (default) or "release"
#   CHOWN_UID/CHOWN_GID  hand artifact ownership to this host user
#
# A release build (BUILD_TYPE=release) is signed with a persistent key that
# android/app/build.gradle reads from the environment (see docs/RELEASING.md):
#   NTRACK_KEYSTORE           path to the keystore (.jks) inside the container
#   NTRACK_KEYSTORE_PASSWORD  keystore password
#   NTRACK_KEY_ALIAS          key alias
#   NTRACK_KEY_PASSWORD       key password
# and honours optional version overrides:
#   NTRACK_VERSION_NAME / NTRACK_VERSION_CODE

set -euo pipefail
cd /work

ABIS="${ABIS:-arm64-v8a}"
BUILD_TYPE="${BUILD_TYPE:-debug}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/work/.docker-target}"
JNILIBS=android/app/src/main/jniLibs

case "$BUILD_TYPE" in
    debug)   GRADLE_TASK=assembleDebug;   VARIANT=debug ;;
    release) GRADLE_TASK=assembleRelease; VARIANT=release ;;
    *) echo "error: BUILD_TYPE must be 'debug' or 'release' (got '$BUILD_TYPE')" >&2; exit 1 ;;
esac

# A release build without a keystore would silently produce an unsigned APK
# (app-release-unsigned.apk) that Android refuses to install — fail loudly.
if [ "$BUILD_TYPE" = "release" ] && [ -z "${NTRACK_KEYSTORE:-}" ]; then
    echo "error: a release build needs a signing key (NTRACK_KEYSTORE et al.)." >&2
    echo "       See docs/RELEASING.md." >&2
    exit 1
fi

if [ "${SKIP_TESTS:-0}" != "1" ]; then
    echo "==> Running Rust test suite"
    cargo test --workspace
fi

# The native libs are always built with the release profile regardless of the
# APK variant: a debug-profile Slint+Skia build is huge and slow, so even the
# "debug" APK ships release-profile .so files.
echo "==> Building Rust native libs (release profile) for ABIs: $ABIS"
rm -rf "$JNILIBS"

# Slint's android backend compiles a small Java helper at build time and
# embeds it as dex. cargo-ndk exports ANDROID_PLATFORM=<minSdk>, which would
# make it look for platforms/android-26; compile against the installed
# platform jar instead (same model as Gradle's compileSdk).
ANDROID_JAR="$(ls -d "$ANDROID_HOME"/platforms/android-*/android.jar 2>/dev/null | sort -V | tail -1)"
if [ -n "$ANDROID_JAR" ]; then
    export ANDROID_JAR
    echo "    using ANDROID_JAR=$ANDROID_JAR"
fi

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

echo "==> Building $VARIANT APK"
ABIS_CSV="${ABIS// /,}"
(cd android && gradle --no-daemon -PntrackAbis="$ABIS_CSV" "$GRADLE_TASK")

mkdir -p dist
OUT="dist/ntrack-${VARIANT}.apk"
cp "android/app/build/outputs/apk/${VARIANT}/app-${VARIANT}.apk" "$OUT"

# Bind-mounted builds run as root; hand the artifacts back to the host user.
if [ -n "${CHOWN_UID:-}" ]; then
    chown -R "${CHOWN_UID}:${CHOWN_GID:-$CHOWN_UID}" \
        dist "$JNILIBS" android/app/build android/.gradle 2>/dev/null || true
fi

echo "==> Done: $OUT"
ls -lh "$OUT"
