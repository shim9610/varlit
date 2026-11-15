#!/usr/bin/env bash
# Verifies the production matrix tile arithmetic with Verus. Exits non-zero
# if the pinned Verus is missing or any proof fails. See README.md.
set -euo pipefail

VERUS_VERSION="0.2026.11.02.8bf68ee"
VERUS_TOOLCHAIN="1.99.1"
# The number of items Verus must report verified. A run that verifies fewer
# (an annotation dropped, a function made external) fails instead of passing
# with less proved.
EXPECTED_VERIFIED=29

here="$(cd "$(dirname "$0")" && pwd)"
verus="${VERUS:-$HOME/.verus/$VERUS_VERSION/verus-x86-linux/verus}"

if [[ ! -x "$verus" ]]; then
    echo "run.sh: Verus not found at $verus (see README.md, or set VERUS=/path/to/verus)" >&2
    exit 2
fi
if ! "$verus" --version | grep -qF "Version: $VERUS_VERSION"; then
    echo "run.sh: $verus is not Verus $VERUS_VERSION" >&2
    exit 2
fi

out="$(mktemp -d)"
trap 'rm -rf "$out"' EXIT
# geometry.rs calls crc32fast in functions Verus does not verify; an empty
# stand-in with the same signature lets the file compile under Verus.
rustc "+$VERUS_TOOLCHAIN" --edition 2024 --crate-type rlib --crate-name crc32fast \
    "$here/stubs/crc32fast.rs" --out-dir "$out"

cd "$here"
log="$out/verus.log"
set +e
"$verus" --edition 2024 --extern "crc32fast=$out/libcrc32fast.rlib" "$@" src/production.rs 2>&1 | tee "$log"
status=${PIPESTATUS[0]}
set -e
if [[ $status -ne 0 ]]; then
    exit "$status"
fi
if ! grep -qxF "verification results:: $EXPECTED_VERIFIED verified, 0 errors" "$log"; then
    echo "run.sh: expected exactly $EXPECTED_VERIFIED verified items and no errors" >&2
    exit 1
fi
