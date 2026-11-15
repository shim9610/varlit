#!/usr/bin/env bash
set -euo pipefail

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."

readonly COMMAND="${1:-}"
if (( $# != 1 )) || [[ -z "$COMMAND" ]]; then
    echo "Usage: $0 COMMAND" >&2
    exit 2
fi

# Local packaging support: execute the same checks offline in an already
# prepared Docker image when rootless Podman is unavailable. No host fallback.
if [[ "${REDB_SANDBOX_ENGINE:-podman}" == docker ]]; then
    : "${REDB_DOCKER_IMAGE:?set a prepared Rust image}"
    : "${REDB_DOCKER_REGISTRY:?set the fetched Cargo registry directory}"
    : "${REDB_DOCKER_TARGET:?set a dedicated writable build directory}"
    : "${REDB_DOCKER_TOOLS:?set a directory containing bin/just for the image}"
    : "${REDB_DOCKER_TOOLCHAIN:?set the installed Rust toolchain}"
    exec docker run --rm --network=none --read-only --cap-drop=all \
        --security-opt=no-new-privileges --pids-limit=2048 --memory=8g \
        --user="$(id -u):$(id -g)" --tmpfs=/tmp:rw,nodev,nosuid,size=2g \
        --mount="type=bind,src=$PWD,dst=/workspace,readonly" \
        --mount="type=bind,src=$REDB_DOCKER_REGISTRY,dst=/cache-registry,readonly" \
        --mount="type=bind,src=$REDB_DOCKER_TARGET,dst=/target" \
        --mount="type=bind,src=$REDB_DOCKER_TOOLS,dst=/tools,readonly" \
        --workdir=/workspace --env=CARGO_BUILD_JOBS=4 --env=CARGO_HOME=/tmp/cargo \
        --env=CARGO_NET_OFFLINE=true --env=CARGO_TARGET_DIR=/target \
        --env=TMPDIR=/target/tmp --env=CARGO_INCREMENTAL=0 --env=CARGO_PROFILE_DEV_DEBUG=0 \
        --env=CARGO_PROFILE_TEST_DEBUG=0 --env=RUSTFLAGS="-D warnings" \
        --env=RUSTUP_TOOLCHAIN="$REDB_DOCKER_TOOLCHAIN" \
        --entrypoint=/bin/bash "$REDB_DOCKER_IMAGE" -c \
        'mkdir -p /tmp/cargo /target/tmp; ln -s /cache-registry /tmp/cargo/registry; export PATH="/tools/bin:$PATH"; exec bash -c "$1"' \
        -- "$COMMAND"
fi

readonly IMAGE="localhost/redb-sandbox:latest"
readonly RUST_VERSION="$(<rust-toolchain)"
readonly TARGET_VOLUME="redb-sandbox-target"

if [[ ! "$RUST_VERSION" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
    echo "Invalid Rust version in rust-toolchain: $RUST_VERSION" >&2
    exit 1
fi

podman build \
    --file Containerfile.sandbox \
    --network=private \
    --pull=missing \
    --build-arg "RUST_VERSION=$RUST_VERSION" \
    --tag "$IMAGE" \
    .

podman volume create --ignore "$TARGET_VOLUME" >/dev/null

podman run --rm \
    --tty \
    --network=none \
    --read-only \
    --cap-drop=all \
    --security-opt=no-new-privileges \
    --pids-limit=2048 \
    --memory=8g \
    --userns=keep-id \
    --user="$(id -u):$(id -g)" \
    --http-proxy=false \
    --tmpfs=/tmp:rw,nodev,nosuid,size=2g \
    --volume=/scratch \
    --volume="$TARGET_VOLUME:/target:rw" \
    --env=CARGO_BUILD_JOBS=4 \
    --env=CARGO_HOME=/tmp/cargo \
    --env=CARGO_NET_OFFLINE=true \
    --env=CARGO_TARGET_DIR=/target \
    --env=HOME=/tmp/home \
    "$IMAGE" \
    bash -c "$COMMAND"
