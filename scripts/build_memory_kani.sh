#!/usr/bin/env bash
# Build the merged upstream crash fixes; do not use a moving branch or version
# string (upstream still calls this source snapshot 0.68.0).
set -euo pipefail
kani_checkout=${1:?usage: build_memory_kani.sh CHECKOUT}
kani_revision=947fcec757d9b6a2d07283a0952aaa088e1d8df1
if [[ ! -e "$kani_checkout" ]]; then
    git init "$kani_checkout"
    git -C "$kani_checkout" remote add origin https://github.com/model-checking/kani.git
fi
git -C "$kani_checkout" diff --exit-code
git -C "$kani_checkout" diff --cached --exit-code
git -C "$kani_checkout" fetch --depth 1 origin "$kani_revision"
git -C "$kani_checkout" checkout --detach "$kani_revision"
test "$(git -C "$kani_checkout" rev-parse HEAD)" = "$kani_revision"
git -C "$kani_checkout" submodule update --init --depth 1 charon
rustup toolchain install nightly-2026-10-21 --profile minimal \
    --component rustc-dev,rust-src,llvm-tools-preview,rustfmt
# CBMC/goto tools come from the separately pinned official 0.68.0 setup.
export PATH="$HOME/.kani/kani-0.68.0/bin:$PATH"
export CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
cd "$kani_checkout"
cargo build-dev -- --release
