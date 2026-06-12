#!/usr/bin/env bash
# ntrack build entrypoint — everything runs in a Docker container, so the
# only host requirement is Docker (or Podman). Tested on Fedora; bind
# mounts use the ":z" SELinux label.
#
# Usage:
#   ./build.sh            build the debug APK  -> dist/ntrack-debug.apk
#   ./build.sh test       run the Rust test suite in the container
#   ./build.sh image      (re)build the builder image only
#   ./build.sh shell      interactive shell in the builder container
#   ./build.sh clean      remove build artifacts (uses the container, since
#                         artifacts may be root-owned)
#
# Environment:
#   ABIS="arm64-v8a armeabi-v7a x86_64"   ABIs to build (default arm64-v8a;
#                                         use x86_64 for the emulator)
#   SKIP_TESTS=1                          skip tests during ./build.sh
#   DOCKER=podman                         force a specific container tool

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
    "$DOCKER" build -t "$IMAGE" docker/
}

run_in_container() {
    # Named volumes cache the cargo registry and gradle artifacts across
    # builds; the repo bind mount carries the rust target dir
    # (.docker-target) so incremental rebuilds are fast.
    "$DOCKER" run --rm \
        -v "$PWD:/work:z" \
        -v ntrack-cargo-registry:/opt/cargo/registry \
        -v ntrack-gradle-home:/root/.gradle \
        -e ABIS="${ABIS:-arm64-v8a}" \
        -e SKIP_TESTS="${SKIP_TESTS:-0}" \
        -e CHOWN_UID="$(id -u)" \
        -e CHOWN_GID="$(id -g)" \
        "$IMAGE" "$@"
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
            "rm -rf /work/.docker-target /work/dist /work/android/app/build /work/android/.gradle /work/android/app/src/main/jniLibs"
        ;;
    *)
        echo "unknown command: $1 (expected: apk | test | image | shell | clean)" >&2
        exit 1
        ;;
esac
