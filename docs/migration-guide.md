# Migration Guide

Varlit migrations are explicit. Normal typed reads reject block version
mismatches; migration code names both the source and target block types and
performs semantic conversion in Rust.

## Upgrading from 0.12.1 to 0.13.0

Set the Cargo dependency requirement to `varlit = "0.13.0"`. Cargo treats
pre-1.0 minor versions as separate compatibility ranges, so `"0.12.1"` does not
select this release. Damaged metadata accepted by older readers may now return
an error.

Files: native files need no conversion. In a format with data checksums,
0.13.0 writes a variable record longer than 64 KiB with per-chunk CRCs under a
new record flag, which 0.12.x refuses. Matrix storage is now tiled and a
matrix spec's schema hash includes its tile shape, so matrix files written by
0.12.x (layout `VMAT` v5) are refused (with `SchemaHashMismatch` when the
format declares a schema hash); recreate them from their source. Stream files
written by 0.13.0 keep their durable boundary in a header block instead of a
`.vks` sidecar (see [Stream state sidecar removed](#stream-state-sidecar-removed));
0.12.x is not tested against them. Files written by a 0.13
pre-release are not guaranteed to open with 0.13.0; matrix files from an
earlier pre-release are already refused with `InvalidMatrixLayout`. Rewrite
them from their source with the release before keeping them. Indexed files (a native
primary with a `.vki` sidecar) are no longer supported; see
[Indexed primary removed](#indexed-primary-removed).

Matrix APIs that tiled storage never used were removed. Delete these from
0.12.x code; unless noted, there is no replacement:

- `ReadLimits::with_matrix_metadata_residency`,
  `with_matrix_metadata_verification`, `with_matrix_generation_cache_bytes` and
  `with_matrix_directory_cache_bytes`; the fields of the same names;
  `effective_matrix_metadata_residency()`,
  `effective_matrix_metadata_verification()` and
  `default_matrix_metadata_cache_bytes()`; and the `MatrixMetadataResidency`
  and `MatrixMetadataVerification` enums.
- `FormatSpec::with_matrix_fatal_forensics()` and the `matrix_fatal_forensics`
  field.
- `compact_matrix()` on `VarlitFile`, `VarlitWriter` and generated writers:
  delete the call. Overwrites do not grow the file.
- `matrix_recovery_report()`: call `verify_matrix_metadata()`, which returns a
  `MatrixRecoveryReport` and can fail.
- `matrix_storage_diagnostics()` and `MatrixStorageDiagnostics`.
- `MatrixRecoveryReport::matrix_last_zero_range_streamed_bytes`,
  `matrix_total_zero_range_streamed_bytes` and
  `matrix_sparse_zeroing_supported`.
- Match arms for `Error::MatrixCommitQuarantined`, `MatrixFatalCorruption`,
  `MatrixChunkNotReopenable`, `MatrixChunkClosed`, `InvalidMatrixChunk` and
  `MatrixDimensionMismatch`.

Matrix behavior changes: open runs no matrix verification; call
`verify_matrix_metadata()`. A damaged tile commit bitmap is
`InvalidMatrixLayout` until `clear_matrix_category` resets it. A matrix reader
that falls behind the write-ahead ring gets `MatrixSnapshotExpired` and must
`follow()`. `matrix_generation_path()` returns the primary path. `diagnose_file`
on a matrix file runs `verify_matrix_metadata()` and reports each finding as
`file.matrix.recovery_finding`, or the error `file.matrix.verification_failed`
if verification fails. See
[durability](durability-model.md#matrix-storage-and-publication).

Typed reads now decode into a value you own instead of returning one; see
[caller-owned decoding](api-changes.md#caller-owned-decoding) for the full
table. The mechanical rewrite:

```rust
// before
let frame = reader.frames()?.get(&key)?.ok_or(Missing)?;
for record in reader.frames()? { let record = record?; use_it(&record); }

// after
use varlit::VarlitEmpty;
let mut frame = Frame::empty();
if !reader.frames()?.get(&key, &mut frame)? { return Err(Missing); }
let mut cursor = reader.frames()?;
let mut record = Frame::empty();
while cursor.next_into(&mut record)? { use_it(&record); }
```

Keep `out` outside the loop so each read can reuse its existing allocations.
To collect a whole block type into a `Vec`, `VarlitFile::decode_blocks_into`
reuses the vector's existing elements. A hand-written `VarlitBlock` that you
read through these APIs needs an `impl VarlitEmpty`; overriding
`decode_varlit_into` is optional and only adds capacity reuse.

`blocks::<T>()` on a `VarlitFile` or `VarlitReader`, and the generated
`<plural>()` accessor of a block without a key, no longer return a
`BlockVec<T>`. They fill one page of a buffer you own and return a `Page` whose
`next` continues the walk; see
[paged reads](api-reference.md#reading-a-block-type-page-by-page). Pick one:

```rust
// before
for point in file.blocks::<Point>()?.iter() { use_it(&point?); }

// after: page by page, nothing allocated per record
let mut payload = Vec::new();
let mut page: Vec<Point> = (0..256).map(|_| Point::empty()).collect();
let mut read = file.blocks::<Point>(&mut payload, &mut page)?;
loop {
    for point in &page[..read.filled] { use_it(point); }
    let Some(next) = read.next else { break };
    read = file.blocks_from::<Point>(next, &mut payload, &mut page)?;
}

// after: keep the BlockVec, through a directory you hold
let points = file.with_directory(&index)?.blocks::<Point>()?;

// after: every record into a Vec
let mut all = Vec::new();
file.decode_blocks_into::<Point>(&mut all)?;
```

Keyed blocks (`keyed_blocks::<T>()` and their generated accessors) and stream
readers keep their accessors.

New read-policy, range-streaming and fallible ownership APIs are optional.
Ordinary opens retain their existing validation policy. Review the
[0.13.0 API changes](api-changes.md#0130) before selecting read checks or adding
allocation-failure retries, particularly after a write may have been published.

Native lazy opens now return metadata errors without automatically scanning.
Use `*_lazy_or_scan[_with_report]` to retain the former behavior, or match the
error and explicitly call `VarlitFile::open_readonly_scan` / `open_scan`.
The same strict policy applies where the library opens lazily for you:
`reopen_readonly()` on a lazily opened handle, and `VarlitWriter`'s lazy
opens. Check those call sites too, not only direct `*_lazy` calls.
Strict lazy open selects header tails when declared, otherwise the trailing
digest. See [lazy-open changes](api-changes.md#lazy-open-and-explicit-scan-selection).

### Stream state sidecar removed

A stream file no longer has a `<file>.vks` companion. Its durable boundary is
in the file: each `sync()` writes its end into a `VPUB` header block, readers
read that end and verify the sync markers after it, and a writer resumes from
the newest anchor's stream summary before it. Nothing to migrate on disk:

- Stream files written by earlier releases open as they are. With no published
  end they are framed from the header once per open; the next writer open
  publishes nothing into them (they have no `VPUB` block), so rewrite a large
  old stream with this release if open time matters.
- An existing `.vks` file is ignored. Delete it at any time.

In code:

- Drop `state: DiskIndexOptions { .. }` from `StreamOptions`; only `limits`
  remains.
- Replace `restore_stream_writer` / `restore_checkpoint_and_open` with
  `open_stream_writer` / `VarlitStreamWriter::open`, which recovers: it keeps
  every acknowledged sync and cuts what follows the last one.
- Delete calls to `bootstrap_stream_checkpoint[_with_progress]`,
  `compact_index` and `release_snapshot`; there is nothing to build, compact or
  release.
- `snapshot_status()` returns `SnapshotStatus { committed_len, records_behind,
  bytes_behind }`: what `follow()` would adopt now. Code that read
  `generation`, `confirmed_generation` or `pinned` has no equivalent.
- Remove matches on `Error::DiskIndex`, `Error::IndexBusy` and
  `Error::PublishedButIndexStale`; stream opens never report lock contention
  between readers, and appends report no index failure.
- `VarlitStreamWriter::create` can return `PublishedButParentSyncPending`
  (naming the stream file) when the file is complete but its directory entry
  could not be synced; a plain `open` resumes it.

### Indexed primary removed

The indexed primary is removed: `VarlitIndexedWriter`, `VarlitIndexedReader`,
the `.vki` sidecar, the generated `create_indexed_writer`,
`open_indexed_writer`, `restore_indexed_writer`, `open_indexed_reader` and
`rebuild_disk_index` constructors, the generated `<Format>IndexedWriter` /
`<Format>IndexedReader` types and their lookups by key (`get_<block>`,
`lookup_entry`, `historical_distinct_keys`), `rebuild_disk_index_with_progress`,
`DiskIndexRebuildReport`, `DiskIndexPlan`, `DiskIndexedBlock`,
`DiskIndexEntry`, `DiskIndexKeyTag`, `VarlitDiskKey`,
`DiskIndexError::PageReclaimed` and `DiskIndexOptions::max_tail_records`.
Updating keyed records grew its files without bound, and it could never
reclaim superseded versions. The replacement design is
[bounded keyed storage](design/bounded-keyed-storage.md); it is not
implemented.

There is no automatic conversion. For each file written through an indexed
writer:

1. Read its data with the release that wrote it.
2. Write it again with this release as a native file (`create_writer`), a
   stream (`create_stream_writer`) or a matrix.
3. Delete the old `.vki` sidecar.

In code:

- Delete `key_index = disk` and `key_index = memory` from block declarations.
  The attribute is now a compile error ("key_index was removed"). A keyed block
  without it keeps working with the resident handles: `push_<block>`,
  `delete_<block>`, `keyed_blocks::<T>()` and `<blocks>()?.get(&key, &mut out)`.
- Replace indexed writers with stream writers for append-only ingest, or with
  resident writers where the application looks records up by key. In a
  `keyed_offset_chain` format the stream writer generates no method for a keyed
  block.
- `rebuild_disk_index` has no replacement: stream files need no rebuild
  (see [Stream state sidecar removed](#stream-state-sidecar-removed)).
- Finite keys (`key_values`, `key_domain`) still generate their key enum and
  work with the resident keyed API.
- A stream writer's Immediate policy counters now start at the first caller
  write; the records `create` writes itself no longer count.

<a id="060-pre-10-wire-changes"></a>

## Pre-1.0 Wire Changes Since 0.6.0

The 0.6.0 changes are wire-breaking under the pre-1.0 policy;
there is no automatic migration path for these surfaces:

- The computed schema hash algorithm moved to version 3
  (`FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`, domain tag
  `varve-schema-v3`). Every computed value changes. Version 3 folds each
  field's resolved codec identity (`VarlitEncode::SCHEMA_ID` /
  `VarlitDecode::SCHEMA_ID`) into the block fingerprint, so two blocks whose
  field types are spelled identically but whose custom codecs emit different
  bytes no longer share a hash. Files whose header pins a v1 or v2 computed
  hash fail open with `SchemaHashMismatch`; recreate them (or re-derive and
  re-pin literal hashes in the declaration). Files with `schema_hash` omitted
  (stored 0) are unaffected because the comparison is disabled.
- `#[derive(VarlitBlock)]` now rejects, at compile time, any field whose codec
  declares no `SCHEMA_ID` (the trait default `0`). Built-in scalars,
  containers and derived blocks always declare one; a hand-written codec used
  as a block field must add `const SCHEMA_ID: u64 = ...;` to both its
  `VarlitEncode` and `VarlitDecode` impls, choosing a value that changes
  whenever its emitted bytes change. Wrapping such a codec in
  `Option`/`Vec`/array/map/tuple does not satisfy the requirement: the
  container fold propagates the missing identity outward.
- The matrix sidecar envelope is version 3. Version-1/2 sidecars are refused
  as `MatrixSidecarMismatch("sidecar version")` and simply regenerate —
  sidecars are regenerable resume state, not data.
- In 0.13.0 the `VMAT` matrix layout (version 5 with `MCRC` version 2 in
  0.12.x) is replaced by tiled matrix storage. The schema hash of a matrix spec
  includes its tile shape, so files written with any `VMAT` layout version are
  refused (with `SchemaHashMismatch` when the format declares a schema hash);
  recreate those files, or convert them as described in
  [converting 0.12 matrix files](#converting-012-matrix-files).
- `PackedBitmap::new`/`get`/`set` and its decode now report
  `Error::InvalidCanonicalEncoding` / `Error::LengthOverflow` and a
  non-matrix allocation resource instead of `Error::InvalidMatrixLayout`, since
  it is an ordinary variable-field codec. Callers matching on
  `InvalidMatrixLayout` for these cases must update.
- A matrix field may now be spelled as a **type alias** of a supported scalar
  (`type Word = u32;`). Eligibility was previously decided by a whitelist of
  literal primitive spellings, which rejected aliases even though the generated
  `SLOT_STRIDE` would have accepted them. Nothing that compiled before stops
  compiling.
- `KeyedMergeEstimate::peak_resident_bytes()` is **renamed** to
  `peak_resident_structural_bytes()` and is documented as a structural estimate
  rather than an upper bound, which is what it always was; its value now also
  includes the output vector reserved while the merge state is alive. The new
  public field `max_output_values_bytes` reports that term. There is no
  compiling alias, deliberately: the old name's contract was false and callers
  must re-read the sizing text rather than silently keep a wrong guarantee.
- `PackedBitmap` is no longer accepted as a fixed-width matrix field (it has no
  encoded width fixed by its type). It remains usable as a variable field, and
  both it and `ChunkedBytes` now declare the stable non-zero codec `SCHEMA_ID`
  that derived fields require — before that they could not be used as derived
  fields at all.
- A `VarlitBlock` whose `ENDIAN`, resolved through `FormatSpec::endian`,
  contradicts the format's own declaration for that block id is now rejected at
  typed registration with `Error::EndianMismatch`. Manual mirrors of a generated
  block must copy its `ENDIAN` alongside its `SCHEMA_FINGERPRINT`.
- The first `sync`/`commit_durable` on a handle that *created* its pathname now
  also fsyncs the parent directory, once per created file, and can return
  `Error::PublishedButParentSyncPending` where it previously returned `Ok(())`.
  Handles that opened an existing pathname are unchanged.
- Decoding a very large `HashMap` under a tight explicit materialization limit
  can now fail with `Error::LimitExceeded`: the charge is the hash table actually
  allocated rather than a per-entry model that under-counted it.
- Stream primaries (and the indexed primaries of those releases) now begin
  with an internal creation-nonce record. Existing primaries keep working, but record counts, sequence numbers,
  and record offsets of newly created primaries shift by one relative to 0.3.0;
  tests or tools that assert absolute counts on a freshly created stream or
  indexed file must account for it.
- The disk-index sidecar metadata record is version 3 and the plan digest
  domain was bumped. A version 2 sidecar is refused as
  `DiskIndexError::MetadataLength` rather than `MetadataVersion`, because
  `decode_metadata` checks the record length before magic and version and the
  record grew from 260 to 300 bytes; a sidecar published against an older plan
  digest is refused as stale. Both recovered with `rebuild_disk_index`, which
  0.13.0 removed together with the sidecars themselves. This is
  hypothetical: `disk_index.rs` is new in 0.4.0, so no released version wrote a
  v2 sidecar.
- Decoding now charges the materialization budget 8 bytes for each distinct
  variable field id above 63. A caller that sized a materialization budget to
  the exact payload byte count *and* uses field ids above 63 must add that
  allowance or it will see
  `Error::LimitExceeded { resource: "variable field ids" }`.
- Generic `push`/`push_info` on a `keyed_offset_chain` format now refuses keyed
  blocks (see the Changelog Breaking entry); switch those calls to
  `push_keyed`/`push_keyed_info`.

## Move Limits To Handle Creation

`limits { ... }` is no longer mandatory and may be partial. Resource policy is
resolved when a reader or writer is opened. Ordinary handles use
`ReadLimits::STANDARD`, which leaves total append length, record count, segment
count, scan length, and cumulative index bytes uncapped while bounding
one-shot payload and materialization work.

Existing `*_with_limits` calls remain tightening-only. Use the new
`*_with_resource_limits` family when runtime policy must raise or lower an
optional format default.

Code that constructs `FormatSpec` with a public struct literal must add the new
`read_limits` field. Prefer `FormatSpec::new(...)`, `FormatSpec::builder()`, or
the generated `Format::spec()` method so future policy additions do not require
editing a literal.

Use visibly named `*_trusted_unbounded` methods only when the complete input
provenance is under your control. Omission now selects the standard runtime
policy rather than causing a compile error.

## Drop The Replace Strategy Argument

`ReplaceStrategy` was removed and `replace` lost its third parameter:

```rust
- writer.replace(index, &block, ReplaceStrategy::FixedCopyOnWrite)?;
- writer.replace(index, &block, ReplaceStrategy::RewriteFile)?;
+ writer.replace(index, &block)?;
```

Delete the argument and the import. There is nothing to choose: `replace` now
reads the format spec and calls `replace_block` for a record-footer format,
`replace_fixed` for a fixed block on a footerless one, and `replace_rewrite`
otherwise. That closes a gap the enum had — on a footer-bearing format both
variants refused, and the route that worked was reachable only by naming
`replace_block` directly.

Two smaller consequences. `replace` now returns the published sequence
(`Result<u64>`) instead of `Result<()>`; ignore it with `let _ =` if you do not
want it. And its bound tightened to `T: VarlitReplaceBlock`, which
`#[derive(VarlitBlock)]` already emits for every block — only a hand-written
`VarlitBlock` impl needs one added, and it is trivially `Ok(())` for an unkeyed
block.

If you want a specific mechanism rather than the replacement, keep calling the
named method; each still refuses where its format cannot serve it.

## Migrate To Resized Replacement

Generated writers now expose `replace_<block>(index, &value)`. The encoded
payload may grow or shrink. Native replacement preserves the target sequence,
rebuilds headers, CRCs, checkpoints, and offset-chain footers, and atomically
publishes a new file generation. Keyed replacement must preserve the key.

## Migrate Fixed Replacement

`ReplaceStrategy::FixedInPlace` was removed in 0.2.0; the whole
`ReplaceStrategy` enum went with it later (see above). Use `replace_fixed`, or
`replace`, for the safe same-size path. These publish a validated replacement
generation atomically, so readers opened before replacement continue to observe
their original snapshot.

The lower-copy path is available only as
`unsafe replace_fixed_in_place_exclusive`. Its caller must prove that no reader
or writer can overlap the operation and must accept that it does not preserve
old snapshots.

## Canonical Map Decoding

Variable-field map decoding now rejects duplicate and non-canonical key order.
Consequently, decoded `HashMap<K, V>` keys require `K: Ord` in addition to the
normal codec bounds. Custom map-like codecs must enforce equivalent duplicate
and ordering rules if they accept hostile input.

## Migrate From 0.1 To 0.2

The 0.2 release intentionally changes three source contracts from 0.1:
`FormatSpec` requires `read_limits`, `ReplaceStrategy::FixedInPlace` is removed,
and `ReplaceStrategy::FixedCopyOnWrite` is added. Facade users of the re-exported
core types are source-affected even when they depend only on `varlit`. These
changes avoid preserving a misleading safe in-place contract. Existing valid
native 0.1 files do not require a wire migration.

## Version Blocks Deliberately

Keep the same block id when the new type represents the same logical block, and
bump the block version when existing bytes no longer decode as the new type.

```rust
use varlit::{VarlitBlock, VarlitMigration};

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 20, version = 1, kind = "fixed")]
struct CounterV1 {
    value: u32,
}

#[derive(Clone, Debug, PartialEq, VarlitBlock)]
#[varlit(id = 20, version = 2, kind = "fixed")]
struct CounterV2 {
    value: u32,
    doubled: u32,
}

struct CounterMigration;

impl VarlitMigration<CounterV1, CounterV2> for CounterMigration {
    fn migrate(from: CounterV1) -> varlit::Result<CounterV2> {
        Ok(CounterV2 {
            value: from.value,
            doubled: from.value.saturating_mul(2),
        })
    }
}
```

`From::ID` and `To::ID` must match. `blocks_migrated` reads records matching
`From::VERSION`, decodes `From`, applies the migration, and returns owned target
values.

```rust
let file = OldFormat::open_readonly("old.varlit")?;
let values = file.blocks_migrated::<CounterV1, CounterV2, CounterMigration>()?;
```

## Publish Migrated Data

`blocks_migrated` does not rewrite the file. To publish a migrated file, create
an output with the new format and push the migrated values.

```rust
let old = OldFormat::open_readonly("old.varlit")?;
let migrated = old.blocks_migrated::<CounterV1, CounterV2, CounterMigration>()?;

let mut out = NewFormat::create("new.varlit")?;
for value in &migrated {
    out.push(value)?;
}
out.flush()?;
out.sync()?;
```

For keyed data, first decide the migration boundary:

- Migrate raw historical puts when every old record should be converted.
- Materialize old keyed state first when tombstones and ops define the intended
  final state.
- Compact before or after migration when the output should drop old ops,
  tombstones, and unrelated block ids.

## Matrix Byte Copy Scaffold

Unchanged fixed-stride matrix cell payloads can be copied without decoding and
re-encoding:

```rust
let mut old = OldMatrixFormat::open_reader("old.varlit")?;
let mut out = NewMatrixFormat::create_writer_with_dims("new.varlit", dims)?;

out.copy_matrix_cell_bytes_from::<OldCell, NewCell>(&mut old, key)?;
```

`OldCell::DIMENSIONS` and `NewCell::DIMENSIONS` must match exactly, and
`SLOT_STRIDE` must be equal. The source cell must be committed; the read path
still verifies enabled source CRC evidence. The target writer writes the bytes
into the target slot and commits the target cell; like any matrix change, the
copy reaches readers at the target's next `sync()`. Semantic conversions,
keyspace iteration, and output publication policy remain caller-owned.

## Field Evolution

For variable blocks, preserve field ids for unchanged fields. Add new fields
with new ids and a valid default when old records may be decoded by the new
type. Use an explicit migration when a field is renamed, split, joined, deleted
without a default, or changes meaning.

Embedded manifests can help inspect what was written, but the caller's
`FormatSpec` remains authoritative for typed reads and migrations.

## Performance Check

Run the ignored smoke tests when a migration changes scan volume, materializes
large keyed state, rewrites output files, or changes codecs used by migrated
blocks:

```powershell
cargo test -p varlit --test perf_smoke -- --ignored --nocapture
cargo test -p varlit --features mmap --test perf_smoke -- --ignored --nocapture
cargo test -p varlit --features mmap,zero-copy --test perf_smoke -- --ignored --nocapture
```

For expensive semantic conversions, add a local migration-sized case and compare
small, medium, and large data before integrating.

## Converting 0.12 matrix files

0.13 cannot open a matrix file written by 0.12.x, and ships no converter. A
file can be converted by reading it with 0.12.1 and writing a new file with
0.13: an application program that depends on both versions at once, under
different names, and copies what the format declares.

```toml
[dependencies]
varlit_012 = { package = "varlit", version = "=0.12.1" }
varlit = { git = "https://github.com/shim9610/varlit", tag = "<0.13 tag>" }
```

Each version's derive macros implement only that version's traits, so every
block type is declared twice, once per crate, with the same ids, versions and
fields. The two format declarations differ only in which crate they come
from. Keep the declarations in two modules, `old` and `new`, so the names do
not clash.

Then, for one file:

1. Open the source with `varlit_012`, read-only.
2. Create the destination with `varlit` and the same dimensions
   (`create_with_dims` for a growing matrix).
3. Copy the ordinary records: for each block type, read every record with the
   0.12 reader (`blocks::<old::T>()`, `keyed_blocks::<old::T>()`) and push the
   matching `new::T` in the same order. Keyed operations and deletes are
   already applied in what 0.12 returns, so only the resulting values are
   written.
4. Copy each matrix block: for every key in the dimensions, ask
   `matrix_cell_status::<old::T>(key)`. For a committed cell, read it with
   `read_matrix_cell::<old::T>(key)`, write the matching value with
   `write_matrix_cell(key, &new_value)` and `commit_matrix_cell::<new::T>(key)`.
   Skip cells that were never committed; 0.13 starts them uncommitted too.
5. Copy each aux region with `matrix_aux_len` and `read_matrix_aux` on the
   source and `write_matrix_aux` on the destination.
6. `sync()` the destination, verify it (`verify_matrix_metadata()`, and the
   generated `check_file` if the application uses it), then rename it over
   the source. Keep the source until the new file has been checked.

```rust,ignore
// Matrix cells of one block, for a two-dimensional matrix of `scans` x `channels`.
let source = old::Format::spec().open_readonly(&path)?;
let mut target = new::Format::spec().create_with_dims(&staging, dims)?;
for scan in 0..scans {
    for ch in 0..channels {
        let key_old = varlit_012::MatrixKey::new(scan, ch);
        if source.matrix_cell_status::<old::Cell>(key_old)?
            != varlit_012::MatrixCellStatus::Committed
        {
            continue;
        }
        let value: old::Cell = source.read_matrix_cell(key_old)?;
        let key = varlit::MatrixKey::new(scan, ch);
        target.write_matrix_cell(key, &new::Cell::from(value))?;
        target.commit_matrix_cell::<new::Cell>(key)?;
    }
}
target.sync()?;
```

`new::Cell::from(old::Cell)` is a field-by-field conversion the application
writes; the two types hold the same fields. Matrix sidecars are resume state
and are not converted: delete them and let the application write new ones.
Cell-by-cell copying reads and writes every committed cell once; for a large
matrix, copy a row at a time with `read_matrix_row_into` instead.
