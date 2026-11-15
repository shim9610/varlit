# Verus pilot: tile slot arithmetic

A pilot that proves three arithmetic properties of the tile layout in
`crates/varlit-core/src/matrix_tiles/geometry.rs` with the
[Verus](https://github.com/verus-lang/verus) verifier, for the whole range
that spec validation admits.

**It proves a transcription of the production arithmetic, not the production
function bodies.** See "Limits" before relying on it.

Nothing here is part of the Cargo workspace. There is no `Cargo.toml`: the
proof is a single file handed to the `verus` binary, and no production crate
depends on Verus.

## Install

Pinned release:

| | |
|---|---|
| Verus version | `0.2026.11.02.8bf68ee` (commit `8bf68ee741df833ea5d0fe6a2255126fdea87e47`) |
| Archive | <https://github.com/verus-lang/verus/releases/download/release/0.2026.11.02.8bf68ee/verus-0.2026.11.02.8bf68ee-x86-linux.zip> |
| sha256 | `f3a0119292df8855e4435da8130bee1873b0e6a60c306accf81a08ea687bba3c` |
| Rust toolchain it requires | `1.99.1-x86_64-unknown-linux-gnu` (via rustup) |
| Z3 | bundled in the archive |

```sh
V=0.2026.11.02.8bf68ee
curl -fsSL -o verus.zip \
  "https://github.com/verus-lang/verus/releases/download/release/$V/verus-$V-x86-linux.zip"
echo "f3a0119292df8855e4435da8130bee1873b0e6a60c306accf81a08ea687bba3c  verus.zip" | sha256sum -c -
mkdir -p "$HOME/.verus/$V" && unzip -q verus.zip -d "$HOME/.verus/$V"
rustup toolchain install 1.99.1-x86_64-unknown-linux-gnu --profile minimal
"$HOME/.verus/$V/verus-x86-linux/verus" --version
```

The checksum is the one observed when this pilot was written; the Verus
project does not publish checksums, so it pins the download, it does not
authenticate it. Disk: about 133 MB for Verus, about 593 MB for the toolchain.
The workspace's own toolchain (MSRV 1.95) is not changed; Verus selects 1.99.1
itself.

## Run

```sh
tools/verus/run.sh
```

Expected last line: `verification results:: 24 verified, 0 errors`. The run
takes about 1.5 seconds. The script exits non-zero if Verus is missing or is
not the pinned version (2), if the transcription has drifted from the
production source (1), or if any proof fails (1). `VERUS=/path/to/verus`
overrides the binary; extra arguments are passed to Verus (for example
`--time`).

## What is proved

File: `src/geometry_proofs.rs`. Preconditions (`BlockGeom::admitted`):
`stride >= 1`, `rows >= 1`, `cols >= 1`, `rows * cols <= 2^20`
(`MATRIX_MAX_TILE_CELLS`), `rows * cols * (stride + 4) <= 64 MiB`
(`MATRIX_MAX_TILE_BYTES`). No smaller bounds are used; `u64` values are
unbounded apart from these.

- (a) `row < rows && col < cols` implies `slot_index(row, col, cols) < rows * cols`,
  and `row * cols + col` does not overflow `u64`. Proved for any
  `rows * cols <= u64::MAX`, which contains the admitted range
  (`slot_index`, `lemma_slot_index_in_range`). `locate` is transcribed too and
  shown to return a slot below `cells()`.
- (b) `slot_index` is injective on `[0, rows) x [0, cols)`
  (`lemma_slot_index_injective`, and `slot_index_injective_exec` on the
  executable function).
- (c) For `slot < cells`: `slot_at(slot) + slot_len() <= tile_bytes()`, and
  `cells`, `slot_len`, `bitmap_len`, `bitmap_unit_len`, `slots_at`,
  `tile_bytes`, `slot_at` do not overflow `u64` (`slot_at`,
  `lemma_slot_within_tile`, `lemma_bounds`).
- The admitted range is not empty: the three extreme shapes from
  `matrix_tiles/tests.rs` satisfy `admitted` (`lemma_admitted_is_inhabited`).

A proof was broken on purpose twice to confirm Verus reports failures
(postcondition of (a) tightened to `< rows * cols - 1`: `23 verified, 1
errors`; precondition of `slot_at` weakened to `slot <= cells`: `22 verified,
2 errors`), then restored.

## Limits

1. **Transcription, not production code.** The executable functions in
   `src/geometry_proofs.rs` are copies of the production bodies. `run.sh`
   checks that each copied expression still appears verbatim in `geometry.rs`
   and that the three constants still have their values in `geometry.rs` and
   `format.rs`. That is a text search. It does not tie an expression to the
   function it sits in, and it does not make Verus check the production file.
2. **Differences from production.** `slot_index` takes an extra ghost (erased)
   argument `rows`, needed to state the bound; `locate` passes it. The struct
   definitions are local copies. The constants are written `0x10_0000` and
   `0x400_0000` instead of `1 << 20` and `64 << 20`.
3. **Two trusted assumptions.** Verus has no specification for
   `u64::div_ceil` and `u64::next_multiple_of`. The file supplies them with
   `assume_specification`, restating their documented behaviour; Verus does
   not check these against std. Properties (a) and (b) do not depend on them;
   (c) does, through `slots_at`.
4. **Callers are not covered.** Nothing proves that production callers pass
   `slot < cells`, or that every `BlockGeom` that reaches these functions
   satisfies `admitted`. Whether spec validation enforces exactly those
   preconditions is taken from the task description, not proved here.
5. **Tool trust.** The result depends on Verus, the bundled Z3, and rustc
   1.99.1 being correct. The production crate is compiled with a different
   rustc.

## Why not the production file itself

Tried with the pinned version; the files are in `experiments/` and are not run
by `run.sh`.

- `option1_include.rs` `#[path]`-includes the production `geometry.rs`, with
  local stand-ins for `crate::{Error, MatrixKey, Result}`, `super::wal` and
  the `crc32fast` crate (`stubs/crc32fast.rs`, passed with `--extern`). It
  compiles under Verus, so the imports are not the blocker. The result is
  `0 verified, 0 errors`: Verus ignores every item outside `verus!` unless it
  is annotated.
- The same file with the deprecated `--no-external-by-default` makes Verus
  look at everything and stops with 38 errors for unsupported items in the
  rest of the file (`from_le_bytes`, `to_le_bytes`, `slice::fill`,
  `div_ceil`, `next_multiple_of`, `trailing_zeros`, `bool::then_some`,
  `TryFromSliceError`, derived `Debug`, `crc32fast::hash`, a `for` loop).
- `option1_plain_fn.rs`: even for one unannotated function,
  `--no-external-by-default` reports `possible arithmetic underflow/overflow`
  on `row * cols + col`. The function needs a `requires`, and a `requires` can
  only be written on the function itself.
- `option1_assume_spec.rs` attaches a spec to the included production
  `slot_index` from outside with `assume_specification`. Verus accepts a
  deliberately false spec (`ensures r == 0`): this mechanism trusts the body,
  it does not verify it.

So with this Verus version, unmodified production bodies cannot be verified
from outside the crate.

## What would close the gap

Annotations in `crates/varlit-core/src/matrix_tiles/geometry.rs`, on the
functions themselves, in plain Rust syntax:

```rust
#[cfg_attr(verus_keep_ghost, verus_verify)]
#[cfg_attr(verus_keep_ghost, verus_spec(r =>
    requires row * cols + col <= u64::MAX,
    ensures r == row * cols + col,
))]
pub(crate) const fn slot_index(row: u64, col: u64, cols: u64) -> u64 {
    row * cols + col
}
```

`verus_keep_ghost` is a cfg that only Verus sets, so ordinary builds see no
attribute and need no new dependency. `experiments/gap_closing_attribute.rs`
verifies under Verus (`1 verified, 0 errors`; with the `ensures` falsified,
`0 verified, 1 errors`) and compiles with plain rustc 1.95.0.

`experiments/annotated_copy.sh` applies such annotations to a temporary copy
of the real `geometry.rs` and verifies it through the `#[path]` shell:
`4 verified, 0 errors`, with the bodies of `cells`, `slot_len` and
`slot_index` verified in place (falsifying one `ensures` gives `3 verified, 1
errors`). What that needed, and what a real change would therefore involve:

- `#[cfg(verus_keep_ghost)] use vstd::prelude::*;` in `geometry.rs`;
- `verus_verify` on `struct BlockGeom` and on `impl BlockGeom`;
- a `verus_spec` on each verified function;
- `#[cfg_attr(verus_keep_ghost, verifier::external)]` on the other functions
  of that `impl` (or moving the verified functions into their own `impl`
  block), because marking the `impl` makes Verus look at all of it;
- `unexpected_cfgs` check-cfg entry for `verus_keep_ghost` in
  `crates/varlit-core/Cargo.toml`, otherwise Cargo warns;
- the lemmas for (a) to (c) would stay in a file like this one and refer to
  the production functions instead of copies.

Not done in that experiment: `slots_at`, `tile_bytes`, `slot_at`, `bitmap_len`
and `locate` were left external (they need the `div_ceil` /
`next_multiple_of` assumptions and a spec for `MatrixKey`), so properties (a)
to (c) were not re-proved against the annotated copy. Verifying the crate
through its real module tree (rather than through the stand-in shell) would
mean running `cargo verus` on `varlit-core` with `vstd` as a cfg-gated
dependency; that was not tried.

One Verus defect met on the way: `assert(...) by (compute)` over a struct with
an array field panics the verifier (`vir/src/interpreter.rs:853`). The file
avoids `by (compute)`.

## CI

A job needs: Linux x86_64, rustup, the archive above (check the sha256),
`rustup toolchain install 1.99.1-x86_64-unknown-linux-gnu --profile minimal`,
then `tools/verus/run.sh`. No Cargo build and no network access during the
run. Caching `~/.verus` and the 1.99.1 toolchain avoids about 730 MB of
downloads and unpacking per run.
