# Verus proofs of the matrix tile arithmetic

`run.sh` runs [Verus](https://github.com/verus-lang/verus) on the production
file `crates/varlit-core/src/matrix_tiles/geometry.rs`. The functions it
verifies are the functions the library compiles: `src/production.rs` includes
that file with `#[path]`, it does not copy it.

## What is proved

For every block shape `FormatSpec::validate` admits (a non-empty tile of at
most 2^20 cells and 64 MiB of slots, stride at least 1) and every 64-bit key:

- `locate` returns a slot index below the tile's cell count, and that slot's
  commit bit lies inside the bitmap;
- `slot_at(slot)` for a slot below the cell count starts after the header and
  bitmap unit and ends inside the tile record;
- `row_run(key)` cells starting at `key`'s slot stay inside the tile;
- two cells of one tile share a slot only when they are the same cell
  (`lemma_slot_index_injective`);
- none of `cells`, `slot_len`, `bitmap_len`, `bitmap_unit_len`, `slots_at`,
  `tile_bytes`, `slot_at`, `slot_index`, `locate`, `row_run` overflows `u64`.

Verus also checks every call between these functions against the callee's
`requires`: `locate` must establish what `slot_index` needs, and so on.

## What is not proved

- **Callers outside `geometry.rs`.** `api.rs`, `writer.rs` and `view.rs` are
  not verified. That they pass `slot_at` a slot obtained from `locate`, and
  runs no longer than `row_run`, is by inspection and by the native tests.
- **That `admitted()` is what validation enforces.** The predicate restates
  the bounds checked in `FormatSpec::validate`; nothing links the two.
- **Two standard-library functions.** Verus has no specification for
  `u64::div_ceil` and `u64::next_multiple_of`. `src/production.rs` assumes
  their documented behaviour (`assume_specification`); Verus does not check
  that against the standard library. The slot byte-range results depend on
  it; the slot index results do not.
- Everything else in the file (tile headers, checksums, the tile cache,
  metadata encoding) is outside verification.

## How the annotations stay out of normal builds

The specifications are attributes behind `cfg(verus_keep_ghost)`, a cfg only
Verus sets, and the model and lemmas are in one `verus! { }` block behind the
same cfg. An ordinary build sees plain functions and has no Verus dependency.

## Install

Verus `0.2026.11.02.8bf68ee`, Linux x86_64:

```sh
curl -LO https://github.com/verus-lang/verus/releases/download/release/0.2026.11.02.8bf68ee/verus-0.2026.11.02.8bf68ee-x86-linux.zip
echo "f3a0119292df8855e4435da8130bee1873b0e6a60c306accf81a08ea687bba3c  verus-0.2026.11.02.8bf68ee-x86-linux.zip" | sha256sum -c
mkdir -p ~/.verus/0.2026.11.02.8bf68ee && unzip -q verus-0.2026.11.02.8bf68ee-x86-linux.zip -d ~/.verus/0.2026.11.02.8bf68ee
rustup toolchain install 1.99.1-x86_64-unknown-linux-gnu --profile minimal
```

The checksum is the one observed when this was set up; Verus publishes none.
Z3 is bundled.

## Run

```sh
tools/verus/run.sh
```

It prints `verification results:: 29 verified, 0 errors` and fails if the
count differs, so removing an annotation cannot pass silently. A run takes
about two seconds.
