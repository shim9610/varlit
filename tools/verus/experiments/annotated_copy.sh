#!/usr/bin/env bash
# Experiment, not part of run.sh: would Verus verify the production function
# bodies in place if geometry.rs carried cfg-gated spec attributes?
# Copies geometry.rs to a temp dir, injects the attributes a real change to
# crates/varlit-core would add, and verifies the copy through the same
# #[path] shell as option1_include.rs. Nothing under crates/ is modified.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
verus="${VERUS:-/root/.verus/0.2026.11.02.8bf68ee/verus-x86-linux/verus}"
src="$here/../../../crates/varlit-core/src/matrix_tiles/geometry.rs"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/crates/varlit-core/src/matrix_tiles" "$work/tools/verus/experiments"
cp "$here/option1_include.rs" "$here/option1_matrix_tiles.rs" "$work/tools/verus/experiments/"
python3 -I - "$src" "$work/crates/varlit-core/src/matrix_tiles/geometry.rs" <<'PY'
import sys
text = open(sys.argv[1]).read()
def inject(text, anchor, attrs, indent="    "):
    assert text.count(anchor) == 1, anchor
    return text.replace(anchor, "".join(indent + line + "\n" for line in attrs) + anchor)
use = "use crate::{Error, MatrixKey, Result};\n"
assert text.count(use) == 1
text = text.replace(use, use + "#[cfg(verus_keep_ghost)]\nuse vstd::prelude::*;\n")
V = "#[cfg_attr(verus_keep_ghost, verus_verify)]"
X = "#[cfg_attr(verus_keep_ghost, verifier::external)]"
def spec(*clauses):
    return [V, "#[cfg_attr(verus_keep_ghost, verus_spec(r =>"] + ["    " + c for c in clauses] + ["))]"]
# The struct and its impl become visible to Verus.
text = inject(text, "pub(crate) struct BlockGeom {", [V], "")
text = inject(text, "impl BlockGeom {", [V], "")
# Three production bodies get a spec and are verified in place.
text = inject(text, "    pub(crate) const fn cells(&self) -> u64 {", spec(
    "requires self.tile[0] * self.tile[1] <= u64::MAX,",
    "ensures r == self.tile[0] * self.tile[1],"))
text = inject(text, "    pub(crate) const fn slot_len(&self) -> u64 {", spec(
    "requires self.stride + 4 <= u64::MAX,",
    "ensures r == self.stride + 4,"))
text = inject(text, "    pub(crate) const fn slot_index(row: u64, col: u64, cols: u64) -> u64 {", spec(
    "requires row * cols + col <= u64::MAX,",
    "ensures r == row * cols + col,"))
# Everything else in the impl is left out of verification in this experiment.
for sig in [
    "    pub(crate) const fn bitmap_len(&self) -> u64 {",
    "    pub(crate) const fn bitmap_at(&self) -> u64 {",
    "    pub(crate) const fn bitmap_unit_len(&self) -> u64 {",
    "    pub(crate) const fn slots_at(&self) -> u64 {",
    "    pub(crate) const fn tile_bytes(&self) -> u64 {",
    "    pub(crate) const fn slot_at(&self, slot: u64) -> u64 {",
    "    pub(crate) const fn locate(&self, key: MatrixKey) -> (TileCoord, u64) {",
    "    pub(crate) fn encode_empty_tile(&self, at: TileCoord, out: &mut [u8]) {",
    "    pub(crate) fn check_tile_header(&self, at: TileCoord, header: &[u8]) -> Result<()> {",
]:
    text = inject(text, sig, [X])
open(sys.argv[2], "w").write(text)
PY
rustc +1.99.1 --edition 2024 --crate-type rlib --crate-name crc32fast \
    "$here/stubs/crc32fast.rs" --out-dir "$work/out"
cd "$work/tools/verus/experiments"
"$verus" --edition 2024 --extern "crc32fast=$work/out/libcrc32fast.rlib" option1_include.rs
