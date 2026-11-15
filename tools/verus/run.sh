#!/usr/bin/env bash
# Verifies src/geometry_proofs.rs with Verus. Exits non-zero if the pinned
# Verus is missing, if the transcribed arithmetic has drifted from the
# production source, or if any proof fails. See README.md.
set -euo pipefail

VERUS_VERSION="0.2026.11.02.8bf68ee"
here="$(cd "$(dirname "$0")" && pwd)"
repo="$here/../.."
verus="${VERUS:-$HOME/.verus/$VERUS_VERSION/verus-x86-linux/verus}"

if [[ ! -x "$verus" ]]; then
    echo "run.sh: Verus not found at $verus (see README.md, or set VERUS=/path/to/verus)" >&2
    exit 2
fi
if ! "$verus" --version | grep -qF "Version: $VERUS_VERSION"; then
    echo "run.sh: $verus is not Verus $VERUS_VERSION" >&2
    "$verus" --version >&2 || true
    exit 2
fi

# Text check, not a proof: every transcribed expression must still appear
# verbatim both in the production source and in the proof file.
geometry="$repo/crates/varlit-core/src/matrix_tiles/geometry.rs"
format="$repo/crates/varlit-core/src/format.rs"
proofs="$here/src/geometry_proofs.rs"
drift=0
need() { # need <file> <fixed string>
    if ! grep -qF -- "$2" "$1"; then
        echo "run.sh: drift: '$2' not found in ${1#"$repo"/}" >&2
        drift=1
    fi
}
for body in \
    'self.tile[0] * self.tile[1]' \
    'self.stride + 4' \
    'self.cells().div_ceil(8)' \
    'self.bitmap_len() + 4' \
    '(TILE_HEADER_LEN + self.bitmap_unit_len()).next_multiple_of(8)' \
    'self.slots_at() + self.cells() * self.slot_len()' \
    'self.slots_at() + slot * self.slot_len()' \
    'row * cols + col' \
    'let [rows, cols] = self.tile;'; do
    need "$geometry" "$body"
    need "$proofs" "$body"
done
need "$geometry" 'pub(crate) const TILE_HEADER_LEN: u64 = 40;'
need "$format" 'pub const MATRIX_MAX_TILE_CELLS: u64 = 1 << 20;'
need "$format" 'pub const MATRIX_MAX_TILE_BYTES: u64 = 64 << 20;'
need "$proofs" 'pub const TILE_HEADER_LEN: u64 = 40;'
need "$proofs" 'pub const MATRIX_MAX_TILE_CELLS: u64 = 0x10_0000;'
need "$proofs" 'pub const MATRIX_MAX_TILE_BYTES: u64 = 0x400_0000;'
if [[ $drift -ne 0 ]]; then
    echo "run.sh: production arithmetic and its transcription differ; update src/geometry_proofs.rs" >&2
    exit 1
fi

cd "$here"
# --no-cheating would also reject the two assume_specification items for
# u64::div_ceil and u64::next_multiple_of, so it is not passed.
exec "$verus" --edition 2024 "$@" src/geometry_proofs.rs
