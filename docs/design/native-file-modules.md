# Native file modules

`crates/varlit-core/src/file.rs` holds `VarlitFile`, `VarlitWriter`,
`VarlitReader` and the record directory types. The code that works on them is
split into files under `crates/varlit-core/src/file/`, in two forms:

- `lifecycle.rs`, `append.rs`, `directory.rs`, `mutation.rs`, `follow.rs`,
  `matrix_access.rs`, `mapped_access.rs` and `header_slots.rs` are pasted into
  `file.rs` with `include!`. They hold `impl` blocks of the handle types and
  share `file.rs`'s scope exactly; `cargo fmt` does not reach them, so format
  them with `rustfmt --edition 2024` directly.
- The others are child modules. Each starts with `use super::*;` and exposes
  its items as `pub(super)`, which `file.rs` brings back into scope with
  `use module::*;`, so moving code there changes no visibility outside `file`.

The comment at the top of each file says what it owns.

This page records the boundaries so that further moves keep them. Moves are
mechanical: a commit that moves code changes no behaviour, and behaviour
changes go in their own commits.

## Who owns what

| Module | Owns |
| --- | --- |
| `file.rs` | The handle types, the record directory and index entries, block chains, segment cursors, keyed merge and compaction, generation validation, and the shared helpers the modules below use |
| `lifecycle.rs` | Create and open: writer, read-only, scan and lazy opens, and the checks each runs first |
| `append.rs` | Appending records, deletes and keyed operations; the keyed tail map |
| `directory.rs` | Reads that start from the directory: metadata, schema manifest, chains, payload reads |
| `follow.rs` | Adopting newer publications, path identity checks, reopening |
| `mutation.rs` | Dead marks, in-place replacement, rewrites, defragmentation, under the mutation permit |
| `replacement_target.rs` | Which record a replacement overwrites |
| `header_codec.rs` | Encoding and decoding the file header, schema manifest, checkpoints and segment payloads |
| `header_slots.rs` | The mutable header-slot and header-tail regions |
| `record_checksum.rs` | The footer checksum of a record, from memory or streamed from a file |
| `record_chunks.rs`, `record_read.rs`, `record_fields.rs`, `record_view.rs` | Reading a record's payload: chunk CRCs, caller buffers, field-by-field decoding, field views |
| `record_file.rs` | The file handle records go through, and its I/O counters for tests |
| `scan.rs` | Framing records during a scan, building the directory, the incremental stream scanner |
| `publication.rs` | Replacement publication: temp files, object identity, rename, directory sync |
| `locks.rs` | Writer exclusion: lock marker, native object lock, the OS calls behind them |
| `matrix_access.rs`, `matrix_tiles_io.rs`, `matrix_sidecar.rs` | Matrix cells and aux bytes, the tile store, and sidecar resume files |
| `mmap_views.rs`, `mapped_access.rs` | Memory-mapped views (`mmap` feature) |
| `tests.rs`, `validation_tests.rs` | Unit tests that need private access |

## Rules for moving code

- Keep mutation permits, lock ordering, durability ordering and error
  propagation as they are. A move must not reorder calls.
- A function that calls a function with a registered proof contract is named in
  `docs/verification/caller-contracts.json` by its module path. Moving it means
  re-recording that file from `scripts/check_proof_bindings.py --observe`; the
  set of caller-to-callee edges must stay the same.
- The deliberate-defect scripts (`scripts/check_*_mutations.py`,
  `scripts/check_publication_protocol.py`) find code by its text and file. If
  moved code contains one of their targets, update the path there.
- Tests keep reaching private helpers through the module tree; do not widen an
  item's visibility to make a move or a test work.

## Still in `file.rs`

`file.rs` is about 7,800 lines, of which roughly 700 are its in-file tests and
the bypass catalogue. The largest remaining groups are the `VarlitWriter` and
`VarlitReader` implementations, the record index entry and directory code, the
streaming rewrite helpers (`rewrite_record_streaming`,
`rewrite_replacement_record_streaming`, `write_staged_record`) and keyed merge
(`merge_keyed_files`, `apply_merge_entries`, `write_keyed_values_atomically`).
The rewrite helpers and keyed merge are the next candidates: each is a group
of free functions with one entry point from the handle types.
