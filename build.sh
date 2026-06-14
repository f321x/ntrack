#!/usr/bin/env bash
# ntrack build entrypoint — everything runs in a Docker container, so the
# only host requirement is Docker (or Podman). Tested on Fedora; bind
# mounts use the ":z" SELinux label.
#
# Usage:
#   ./build.sh            build the debug APK          -> dist/ntrack-debug.apk
#   ./build.sh release    build the signed release APK -> dist/ntrack-release.apk
#   ./build.sh keystore   generate the release signing keystore in the builder
#                         image (no host JDK needed) -> release-signing/
#   ./build.sh test       run the Rust test suite in the container
#   ./build.sh image      (re)build the builder image only
#   ./build.sh shell      interactive shell in the builder container
#   ./build.sh clean      remove build artifacts (uses the container, since
#                         artifacts may be root-owned)
#
# Environment:
#   ABIS="arm64-v8a armeabi-v7a x86_64"   ABIs to build (default arm64-v8a;
#                                         use x86_64 for the emulator)
#   SKIP_TESTS=1                          skip tests during a build
#   SKIP_IMAGE_BUILD=1                    reuse an existing builder image
#                                         (CI pre-builds it with layer caching)
#   DOCKER=podman                         force a specific container tool
#
# Release signing (./build.sh release) — see docs/RELEASING.md:
#   NTRACK_KEYSTORE           host path to the keystore (.jks)
#   NTRACK_KEYSTORE_PASSWORD  keystore password
#   NTRACK_KEY_ALIAS          key alias
#   NTRACK_KEY_PASSWORD       key password
#   NTRACK_VERSION_NAME / NTRACK_VERSION_CODE   optional version overrides

set -euo pipefail
cd "$(dirname "$0")"

if [ -z "${DOCKER:-}" ]; then
    if command -v docker >/dev/null 2>&1; then
        DOCKER=docker
    elif command -v podman >/dev/null 2>&1; then
        DOCKER=podman
    else
        echo "error: docker (or podman) is required" >&2
        exit 1
    fi
fi

IMAGE=ntrack-builder

build_image() {
    if [ "${SKIP_IMAGE_BUILD:-0}" = "1" ]; then
        echo "==> SKIP_IMAGE_BUILD=1: reusing existing '$IMAGE' image"
        return 0
    fi
    "$DOCKER" build -t "$IMAGE" docker/
}

run_in_container() {
    # Named volumes cache the cargo registry and gradle artifacts across
    # builds; the repo bind mount carries the rust target dir
    # (.docker-target) so incremental rebuilds are fast.
    local mounts=(
        -v "$PWD:/work:z"
        -v ntrack-cargo-registry:/opt/cargo/registry
        -v ntrack-gradle-home:/root/.gradle
    )
    local envs=(
        -e ABIS="${ABIS:-arm64-v8a}"
        -e SKIP_TESTS="${SKIP_TESTS:-0}"
        -e BUILD_TYPE="${BUILD_TYPE:-debug}"
        -e NTRACK_VERSION_NAME="${NTRACK_VERSION_NAME:-}"
        -e NTRACK_VERSION_CODE="${NTRACK_VERSION_CODE:-}"
        -e NTRACK_KEYSTORE_PASSWORD="${NTRACK_KEYSTORE_PASSWORD:-}"
        -e NTRACK_KEY_ALIAS="${NTRACK_KEY_ALIAS:-}"
        -e NTRACK_KEY_PASSWORD="${NTRACK_KEY_PASSWORD:-}"
        -e CHOWN_UID="$(id -u)"
        -e CHOWN_GID="$(id -g)"
    )
    # A release build needs the signing keystore inside the container; mount it
    # read-only at a fixed path (never under /work, so it can't leak into the
    # repo bind mount) and point Gradle at that path.
    if [ -n "${NTRACK_KEYSTORE:-}" ]; then
        local ks_abs
        ks_abs="$(readlink -f "$NTRACK_KEYSTORE")"
        if [ ! -f "$ks_abs" ]; then
            echo "error: NTRACK_KEYSTORE='$NTRACK_KEYSTORE' is not a file" >&2
            exit 1
        fi
        mounts+=( -v "$ks_abs:/run/secrets/ntrack-release.jks:ro" )
        envs+=( -e NTRACK_KEYSTORE=/run/secrets/ntrack-release.jks )
    fi
    "$DOCKER" run --rm "${mounts[@]}" "${envs[@]}" "$IMAGE" "$@"
}

case "${1:-apk}" in
    image)
        build_image
        ;;
    apk)
        build_image
        run_in_container scripts/build-apk.sh
        echo
        echo "Install with: adb install -r dist/ntrack-debug.apk"
        ;;
    release)
        build_image
        BUILD_TYPE=release run_in_container scripts/build-apk.sh
        echo
        echo "Signed release APK: dist/ntrack-release.apk"
        ;;
    keystore)
        build_image
        run_in_container scripts/gen-release-keystore.sh
        ;;
    test)
        build_image
        run_in_container cargo test --workspace
        ;;
    shell)
        build_image
        "$DOCKER" run --rm -it \
            -v "$PWD:/work:z" \
            -v ntrack-cargo-registry:/opt/cargo/registry \
            -v ntrack-gradle-home:/root/.gradle \
            "$IMAGE" bash
        ;;
    clean)
        build_image
        run_in_container bash -c \
            "rm -rf /work/.docker-target /work/dist /work/android/app/build /work/android/build /work/android/.gradle /work/android/app/src/main/jniLibs"
        ;;
    *)
        echo "unknown command: $1 (expected: apk | release | keystore | test | image | shell | clean)" >&2
        exit 1
        ;;
esac
