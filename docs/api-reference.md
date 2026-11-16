# Varlit API Reference

This is a practical public API map for application authors. It is not a
rustdoc replacement; it explains which API to reach for and what it means.

This page describes Varlit 0.13.0, including read validation selection, range
streaming and fallible ownership. See [API changes](api-changes.md#0130).

## Read validation

`ReadLimits::with_read_validation` selects optional read-side checks independently
of the checksum policy stored in the format. Data checksums use cyclic redundancy
checks (CRCs).

| Selection | Data CRC | Additional structure checks |
| --- | --- | --- |
| Unset (`None`) | Existing format behavior | Existing backend behavior |
| `ReadValidation::FULL` | On | On |
| `ReadValidation::CRC_ONLY` | On | Off |
| `ReadValidation::STRUCTURE_ONLY` | Off | On |
| `ReadValidation::TRUSTED` | Off | Off |

For a runtime override that preserves the format's other resource settings:

```rust,no_run
# fn open(spec: varlit::FormatSpec, path: &std::path::Path) -> varlit::Result<varlit::VarlitReader> {
use varlit::{ReadLimits, ReadValidation};
let limits = ReadLimits::MISSING.with_read_validation(ReadValidation::TRUSTED);
spec.open_reader_with_resource_limits(path, limits)
# }
```

For stream handles, set `StreamOptions::limits` to that same `limits` value.
`overlay` replaces an existing selection only when the new one is explicit;
`tighten` keeps an existing declaration. `effective_read_validation(integrity)`
resolves record-payload checks; backend admission also distinguishes the unset
compatibility case. An explicit CRC-on selection rejects a checksum-free format
with `InvalidFormatSpec`; reading stored CRCs requires the `integrity` feature.

Every mode retains extent/overflow checks, resource and decoder limits, finite
traversal and committed-publication guards. Protocol/generation checksums remain.
`TRUSTED` neither authorizes `TrustedUnbounded` nor expands a resource budget.
Skipped checks may allow wrong but well-formed data to be returned. Explicit
`verify_all`, recovery and index reconstruction retain their strict admission
contracts; standalone `ChunkedBytes` checks remain independent. See the
[read-policy contract](design/read-validation-policy-review.md) for framing I/O
and the [mmap contract](#mmap-and-zero-copy) for backing-file requirements.

## Fallible ownership

`TryClone::try_clone(&self) -> Result<Self>` creates an owned copy using fallible
allocations. Refusal releases partial output and leaves the source unchanged.
There is no blanket implementation for `T: Clone`: user implementations and any
hash/equality callbacks must uphold the same contract. Existing public `Clone`
implementations keep their original behavior.

Supported values include `Vec`, `String`, `PathBuf`, `HashMap`, `VecDeque`, `Box`,
`Option`, scalar/copy arrays, and Varlit's `BlockVec`, `KeyedBlockVec`,
`ChunkedBytes`, `PackedBitmap`, `MatrixDimensions` and
`LayoutValue`. Shared owners clone their reference count without allocating.
`BTreeMap` has no `TryClone` implementation in this change.

`ScanCancellationToken::try_new() -> Result<ScanCancellationToken>` reports
shared-owner allocation refusal. Token `TryClone` shares the existing flag;
it does not reset cancellation. `new` and `default` remain infallible APIs.

`try_box(value) -> Result<Box<T>>` reports storage refusal. It consumes the input
and drops it normally on failure; clone the source fallibly first if the source
must remain available. `try_box_with(|| -> Result<T> { ... })` reserves the Box
before invoking the initializer. If reservation fails, the initializer is not
called; if initialization returns an error, the storage is released and that
error is returned. `Box<T>::try_clone` follows this reservation-first order.
The initializer is consumed even when it is not called, so its owned captures
are dropped normally; borrow the source if it must remain available for retry.

`try_box_uninit::<T>() -> Result<Box<MaybeUninit<T>>>` reserves storage without
constructing `T`. Use `Box::write` to initialize it safely, or write in place and
uphold `assume_init`'s safety requirements. This is useful for large values that
should not first be constructed on the stack. The `with` callback still returns
its value by value and does not remove arbitrary stack temporaries. Dropping an
uninitialized Box releases its storage without dropping a `T`.
If caller-controlled initialization writes owned fields in stages, the caller
must also clean up those initialized fields when abandoning that initialization.

The caller can release other storage and retry after refusal; the helpers impose
no retry policy. Initializers, user `TryClone` implementations and destructors
must themselves return normally. A `Result` signature cannot convert their
panics, aborts or nontermination into an allocation error.

Internal shared file/page owners use fallible allocation without changing public
`std::sync::Arc` types or persisted file formats.

Direct buffer/box refusal uses `Error::AllocationFailed`; internal shared-owner
refusal can surface as `Error::Io` with `std::io::ErrorKind::OutOfMemory`.
After publication, also inspect `Error::ErrorContextAllocationFailed` before
retrying, because its inline context can indicate an already completed or
uncertain operation.

These are scoped allocation-refusal contracts, not a global OOM guarantee. See
[allocation limitations](known-limitations.md#72-some-allocations-abort-instead-of-returning-an-error).

## Generated Format API

For a format declared as:

```rust
varlit_format! {
    pub format AppFormat {
        magic: b"APP";
        version: 1;
        blocks {
            fixed Point(id = 1) { x: u32, y: u32 }
            variable User(id = 2, key = [id]) { id: u64, name: String }
        }
    }
}
```

the macro generates:

| Item | Purpose |
| --- | --- |
| `AppFormat` | zero-sized namespace for the format |
| `AppFormat::spec()` | returns the static `FormatSpec` |
| `AppFormat::create(path)` | create/truncate and return raw `VarlitFile` |
| `AppFormat::create_writer(path)` | create/truncate and return typed writer |
| `AppFormat::open(path)` | open read-write raw `VarlitFile` |
| `AppFormat::open_writer(path)` | open read-write typed writer |
| `AppFormat::open_readonly(path)` | open raw read-only `VarlitFile` |
| `AppFormat::open_reader(path)` | open typed snapshot reader |
| `AppFormat::open_reader_with_limits(path, limits)` | open with limits tightened below the declaration |
| `AppFormat::open_reader_with_resource_limits(path, limits)` | open with runtime policy that may raise or lower optional defaults |
| `AppFormat::open_reader_trusted_unbounded(path)` | explicit trusted-input open; never called by ordinary open |
| `AppFormat::open_writer_with_limits(path, limits)` | writer open with field-wise tighter limits |
| `AppFormat::open_writer_with_resource_limits(path, limits)` | writer open with runtime resource-policy override |
| `AppFormat::open_writer_trusted_unbounded(path)` | explicit trusted-input writer open |
| `AppFormat::open_recover(path)` | explicit recovery open |
| `AppFormat::open_recover_with_report(path)` | recovery open plus report |
| `AppFormat::diagnostics()` | static format diagnostics |
| `AppFormat::diagnose_file(path)` | read-only file diagnostics |
| `AppFormat::self_test(path)` | build an end-to-end self-test |
| `AppFormat::check_file(path)` | explicit typed inspection with automatic block registration |
| `AppFormat::check_file_with_resource_limits(path, limits)` | inspect with finite caller-selected resource limits |

`AppFormat::create` and `create_writer` create-or-truncate the path. For callers
that must prove they created and own the file (diagnostic scaffolding, self-test
scaffolding) and must never destroy caller data, `VarlitFile::create_new(spec, path)`
and `VarlitWriter::create_new(spec, path)` open with `create_new`: they never
truncate or reuse an existing path and fail with an `AlreadyExists` I/O error if
one exists. Matrix formats have the same pair —
`VarlitFile::create_new_with_dims(spec, path, dims)` and
`VarlitWriter::create_new_with_dims` — which keep the one exclusively created,
lock-bound handle from claim through matrix initialization, with no pathname
re-open window in between.

Generated typed methods depend on block names:

| Declaration | Writer method | Reader method |
| --- | --- | --- |
| `fixed Point` | `push_point(&Point)`, `replace_point(index, &Point)` | `points() -> BlockVec<Point>` |
| `variable User(key=[id])` | `push_user(&User)`, `replace_user(index, &User)`, `delete_user(&key)` | `users() -> KeyedBlockVec<K, User>` |
| `matrix Cell` | `write_cell(key, &Cell)`, `commit_cell(key)` | `cell(key)`, `cell_status(key)` |
| `aux { thumbnail: 16 }` | `write_thumbnail_aux(offset, bytes)` | `read_thumbnail_aux(offset, len)` |

## FormatSpec

`FormatSpec` is the runtime registry and file contract.

| API | Use |
| --- | --- |
| `FormatSpec::new(...)` | manual/derive-first spec construction |
| `FormatSpec::builder()` | builder-style construction |
| `with_extension(ext)` | recommended file extension metadata |
| `with_commit_policy(policy)` | append-log commit policy |
| `with_compression_policy(policy)` | global variable-block compression |
| `with_block_compression(descriptors)` | per-variable-block record-explicit compression |
| `with_computed_schema_hash()` | pin computed schema hash into the header contract |
| `with_read_limits(limits)` | set declaration-level resource policy without changing schema identity |
| `with_resource_limits(limits)` | resolve standard/default policy and overlay runtime values |
| `tighten_read_limits(limits)` | component-wise meet; a finite ceiling can never be widened |
| `with_matrix_spec(dims, commits, blocks)` | manual matrix registry |
| `with_matrix_aux(aux)` | manual matrix aux registry |
| `with_block_identities(identities)` | attach the per-block identity table (endian override, keyedness, generated codec fingerprint) hashed by `computed_schema_hash()`; generated `spec()` does this automatically |
| `validate()` | check static spec consistency, including duplicate/unregistered block identities |
| `computed_schema_hash()` | deterministic schema fingerprint (algorithm version `FormatSpec::SCHEMA_HASH_ALGORITHM_VERSION`, currently 3) |
| `effective_layout()` | physical layout plan using header/segment/lead-in/raw/footer vocabulary |
| `inspect_layout_file(path)` | validate a native or custom file and return physical layout ranges |
| `schema_debug_dump()` | human-readable schema dump |
| `diagnostics()` | structured static diagnostics |
| `diagnose_file(path)` | structured file diagnostics |
| `self_test(path)` | self-test builder |

Use the generated `Format::spec()` path unless you need derive-first or manual
registry construction.

The computed schema hash covers wire layout, not just field membership: fields
are hashed in declaration order with their encoding ordinal and a field-count
frame, and each block's endian override, keyedness, and generated codec
fingerprint are folded in through `block_identities`. Reordering same-typed
field declarations, changing a per-block endian, or changing a codec identity
therefore changes the hash. Omitting `schema_hash` in a declaration stores 0
and disables the open-time comparison; use `schema_hash: computed` (or pin a
literal derived from `computed_schema_hash()`) whenever schema locking is
wanted.

### ReadLimit And ReadLimits

`ReadLimit` is `Missing`, `Finite(u64)`, or `TrustedUnbounded`. `ReadLimits`
contains operational policy fields documented in the format declaration guide.
Missing fields are resolved when a handle opens. Ordinary
APIs return `Error::MissingResourceLimit` only for an explicitly unresolved policy and
`Error::TrustedUnboundedRequiresExplicitApi` when a trusted policy is presented
to an ordinary entrypoint. Values above a finite ceiling return
`Error::LimitExceeded` before a claim-sized allocation or read begins.

`ReadLimits::missing()` is a partial-overlay builder base,
`ReadLimits::finite_all(n)` sets every ceiling to `n`, and the const
`with_max_*` builders set individual finite values. `ReadLimits::STANDARD`
places no total ceiling on append-log file length, scan length, record count,
segment count, or cumulative index bytes; it limits one-shot payload decoding
and materialization. Compatibility `*_with_limits` methods perform a meet, so
`Finite(64 MiB)` tightened by `Finite(16 MiB)` is `Finite(16 MiB)`.
`*_with_resource_limits` overlays fields and may raise or lower defaults.

### Walking a chain

Two chains are walkable, both `&self`, both lazy — each holds one offset, so a
chain longer than memory is still walkable and the cost is the hops rather than
the file.

| method | follows | refuses without |
| --- | --- | --- |
| `block_chain(block_id)` -> `BlockChain<'_>` | `prev_same_block_offset` | `index: [block_offset_chain]` |
| `keyed_chain(from)` -> `KeyedChain<'_>` | `prev_same_key_offset` | `index: [keyed_offset_chain]` |

Both yield `Result<RecordIndexEntry>`. `record_entry_at(record_offset)` frames
one record into its entry without a walk — it is what `read_block_at` builds and
discards, and it is what a caller steering their own walk needs, since the entry
is where the next offset lives.

```rust
let newest = file.key_tail_offsets::<Reading>()?[&sensor];
let (mut scratch, mut reading) = (Vec::new(), Reading::empty());
for step in file.keyed_chain(newest)? {
    let entry = step?;
    if entry.block_id == Reading::ID {
        file.read_block_at::<Reading>(entry.record_offset, &mut scratch, &mut reading)?;
        history.push(reading.clone());
    }
}
```

**`keyed_chain` crosses block ids; `block_chain` does not.** That is the one
behavioural difference and the reason they are separate types. A delete writes
its tombstone under `TOMBSTONE_BLOCK_ID`, and that tombstone carries a live keyed
predecessor and becomes the key's new tail, so a keyed walk that refused it would
stop at the first deleted generation and look like an answer. Filter on
`entry.block_id` yourself.

Merge ops are **not** on this chain. `push_op` appends under `OP_BLOCK_ID` with
`prev_same_key_offset` set to `None` and does not move the key's tail, so
`keyed_chain` never surfaces one — a history reconstructed from the walk alone
omits every op-applied mutation.

If you build the walk by hand from a materialised index instead, take it with
`index_entries_into` — **not** `block_entries_into::<T>`, and not the `entries`
half of `keyed_blocks_into`. Both filter to `T::ID`, which drops exactly the
tombstone and op hops the chain runs through.

Each step is charged against `max_records`, and both chains must strictly
decrease, so a crafted or damaged file cannot make a walk loop.

`ReadLimits::UNTRUSTED` (`ReadLimits::untrusted()`) is the finite companion to
`STANDARD` for input from untrusted sources. Every aggregate dimension that
`STANDARD` leaves effectively unbounded is finite: `max_records` 16,000,000,
`max_scan_bytes` 16 GiB, `max_index_bytes` 1 GiB,
`max_segments` 65,536, and `max_keyed_tail_bytes` 256 MiB, inheriting the
`STANDARD` per-item caps for everything else — including
`max_file_header_extension_len`, which is 64 KiB in both. Use it when a resident open must not let a hostile file choose the reader's
CPU, I/O, or memory; large trusted files should use the scalable APIs or explicit
wider limits instead.

### Matrix metadata

`MatrixMetadataResidency`, `MatrixMetadataVerification` and the `ReadLimits`
fields, setters and accessors for them
(`matrix_metadata_residency`, `matrix_metadata_verification`,
`matrix_generation_cache_bytes`, `matrix_directory_cache_bytes`, their `with_*`
setters, `effective_matrix_metadata_residency()`,
`effective_matrix_metadata_verification()` and
`default_matrix_metadata_cache_bytes()`) were removed in 0.13.0. Tiled matrix
storage never read them. The `max_matrix_*` limits remain and are enforced at
open.

A tiled reader keeps no commit-bitmap cache. It reads a tile's commit bitmap
with the cells it is reading and checks the bitmap's CRC32 on every read. What
it does keep is a fixed-size table of resolved tile positions and, for its
snapshot, an overlay of the ranges published since the last checkpoint (see
[durability](durability-model.md#what-readers-see)).

No open runs a matrix verification pass.
`verify_matrix_metadata()` on a reader, writer or `VarlitFile` is the
verification pass. It visits every tile that exists in the handle's view,
checks each tile's header and commit bitmap against their CRC32s, and returns
a `MatrixRecoveryReport` with a `Recoverable` `CommitMap` finding per damaged
tile and a `ClearCategory` recommendation per affected category. It reports and
changes nothing. `matrix_recovery_report()` was removed in 0.13.0. See
[recovery](recovery-model.md).

`max_keyed_tail_bytes` (`limits { keyed_tail: ... }`) now bounds the nominal
in-memory batch of each writer's keyed-tail cache. At the boundary the writer
spills entries to its own temporary paged index and continues. Spilling does
not sync or publish the data file and adds no shared cache or writer lock.
Both generated and generic keyed writers use this path. One encoded key plus
fixed tree-page scratch is the minimum work unit; map allocator overhead and
temporary serialization are additional to the nominal charge. The default
working batch is capped at 2 MiB per block even when the declared ceiling is
larger. Scratch I/O failures are returned before append. Temporary files are
removed when their owning handle is dropped.

`key_tail_offsets` explicitly returns an owned `HashMap`; that operation still
refuses a result exceeding its materialization budget. Writer construction
builds the spillable directory directly instead of calling this owned-map API.
Priming each keyed block still walks the input directory (or its block chains
for a directoryless handle). Use stream writers for bounded ingest without the
resident input directory or the per-block priming scans.

## Reader And Writer Handles

| Type | Role |
| --- | --- |
| `VarlitReader` | read-only snapshot wrapper |
| `VarlitWriter` | write-capable wrapper |
| `VarlitFile` | lower-level read/write type used by both wrappers |

Native handles may scan records at open, but directory and scan buffers are
bounded by an explicit finite `max_index_bytes`; the unbounded default uses
2 MiB per buffer. Excess rows use private
temporary files. An ordinary private directory row is 13 bytes: an offset,
block ID and committed-state byte. Eligible rewritten directories use compressed
private rows within the existing allowance; full entries are reconstructed from
record headers/footers on demand. Fixed row/page workspace
is additional. Open I/O still scales with records when no usable fast metadata
route applies. Reordered sequences use a temporary uniqueness index.

Two other ways to pay for that directory, both of which cost the handle
nothing: hold it yourself (`open_readonly_without_directory`, below), or build
only the part your question needs (`record_map`, below).

**Three policies avoid the open-time scan; two that look as though they might
do not.**

- `IndexPolicy::segment_on_flush` **does** avoid it. Each commit point appends an
  internal *segment* record covering the records that commit point added, chained
  to the previous one through the record footer's `prev_same_block_offset`. Open
  confirms a record footer at the end of the file and walks that chain backwards,
  reading one record per commit point instead of scanning data-record headers.
  The resulting directory uses the same bounded-buffer/spill policy as a scan.
  A chain whose decoded staging exceeds the current buffer falls back to bounded
  record framing. Segment payloads add disk storage proportional to their covered
  record count. **A writing session must end with `flush` or `commit`** — a data record at the end of the file leaves open
  no chain to start from, and it silently scans instead. See [Known Limitations
  §2.1](known-limitations.md#21-varlitfile-scans-the-whole-file-at-open-and-holds-a-record-index)
  for the full table, the other costs, and every fallback.
- `IndexPolicy::open_digest_on_flush` avoids it **completely**, by not building
  an index at all. See [An open that reads no
  record](#an-open-that-reads-no-record) below: `open_readonly_lazy` frames one
  record, the digest, and the caller builds whatever index it needs with
  `record_map`. Its on-disk cost does not grow with the record count;
  `segment_on_flush` is the only one of the three whose does.
- `IndexPolicy::header_tails` avoids it too, from the file header rather than
  from a record: the same three facts, in a fixed region of the header that is
  rewritten in place at the end of every commit point, so nothing appended can
  move it or bury it. `open_readonly_lazy` tries it before the digest and
  reports `LazyOpenSource::HeaderTails`; the handle keeps no directory either,
  and the open frames the commit marker the table names, whatever closes the
  same commit point, and one record per block tail. The region is sized by the
  block count alone — 360 bytes for a two-block format. It requires
  `block_offset_chain` (which it turns on for you), `integrity: crc32` and a
  `transaction_marker` commit policy, and is refused both alongside
  `open_digest_on_flush` and on a format declaring matrix blocks. See [Format
  Author Guide](format-author-guide.md#when-a-later-append-must-not-be-able-to-hide-the-answer).
- `IndexPolicy::CheckpointOnFlush` does **not** seed an open from a checkpoint.
  Every open without the segment chain calls `load_index` → `scan_records_range`,
  which walks from the header to the file length; a checkpoint met on the way is
  validated and its decoded entries are discarded. What the policy bounds is
  writer-side checkpoint bytes (it spaces full checkpoints geometrically), not
  open cost. `docs/spec.md` describes a checkpoint-seeded open as a design
  target; it is not implemented.
- The `scan_on_open` flag is **not** a behaviour switch — it is folded into the
  schema manifest and hash bytes and is consulted nowhere else in `varlit-core`, so
  clearing it does not produce a non-scanning open.

`ReadLimits::STANDARD` leaves `max_records`, `max_index_bytes` and
`max_scan_bytes` at `u64::MAX`, so the default profile places no ceiling on
resident index size; use `ReadLimits::UNTRUSTED` for input you did not produce.
`max_file_len` is accepted and inert — nothing enforces a file-length ceiling;
see [Known Limitations](known-limitations.md).

For bounded-directory workloads, use [Scalable Stream
Handles](#scalable-stream-handles) below, available by default. `VarlitReader` is a snapshot as of its own open: records
another handle appends afterwards are not visible until it advances with
`follow()` or is reopened. See
[Known Limitations §2.1](known-limitations.md#21-varlitfile-scans-the-whole-file-at-open-and-holds-a-record-index).

### Reads that fill a buffer you own

Every typed read decodes into a value you own. `get`, `cursor().next_into`,
`StreamingBlocks::next_into`, `read_block_at`, `decode_block_into`,
`read_header_block`, `read_matrix_cell` and their generated
wrappers take `out: &mut T` and decode into it, reusing the `Vec`s and `String`s
it already owns. Start from `T::empty()` (`VarlitEmpty`) or any existing value
and keep it across a loop:

```rust
let mut cursor = reader.frames()?;
let mut frame = Frame::empty();
while cursor.next_into(&mut frame)? {
    consume(&frame); // borrow it; the next step decodes over it
}
```

A `bool` result means present (`true`) or absent / end of walk (`false`, `out`
untouched). After an error `out` is valid but its contents are unspecified. The
in-place decode is charged the same materialization budget as a fresh decode and
refused at the same limit.

The collection reads listed below accept reusable caller-owned buffers.
`out` is cleared and then filled. Reusing sufficient capacity can avoid growing
the outer buffer, but larger results may reserve more space, and decoded values,
metadata entries and internal scratch may allocate separately. These `Vec` and
`HashMap` APIs do not impose a fixed-capacity, no-growth contract.

| allocating | caller-supplied |
| --- | --- |
| `index_entries()` | `index_entries_into(&mut Vec<RecordIndexEntry>)` |
| `all_metadata()` | `all_metadata_into(&mut Vec<(String, Vec<u8>)>)` |
| `blocks::<T>()` | `block_entries_into::<T>(&mut Vec<RecordIndexEntry>)` |
| — | `decode_blocks_into::<T>(&mut Vec<T>)` — decodes **every** matching record into a growable output, decoding into the elements it already holds; not a bounded-memory iterator |
| `blocks_migrated::<F, T, M>()` | `blocks_migrated_into::<F, T, M>(&mut Vec<T>)` |
| `keyed_blocks::<T>()` | `keyed_blocks_into::<T>(&mut Vec<RecordIndexEntry>, &mut HashMap<..>)` |
| `materialized_keyed_blocks::<T>()` | `materialized_keyed_blocks_into::<T>(&mut HashMap<..>)` |
| `key_tail_offsets::<T>()` | `key_tail_offsets_into::<T>(&mut HashMap<..>)` |
| `open_readonly(spec, path)` | `open_readonly_with_scratch(spec, path, &mut Vec<RecordIndexEntry>)` |
| `open(spec, path)` | `open_with_scratch(spec, path, &mut Vec<RecordIndexEntry>)` |

`decode_blocks_into` is deliberately **not** named as the twin of `blocks()`:
`blocks()` returns a lazy collection whose peak is one payload however many
records it covers, and this one materialises all of them.

The generated per-block accessors carry the same pair — `<plural>_entries_into`,
`<plural>_decoded_into`, and `<plural>_into` for a keyed block — on both the
inherent and the trait route.

Reading one record you already hold an entry for, into a buffer you own:

| API | Meaning |
| --- | --- |
| `read_payload_into(&entry, &mut Vec<u8>)` | stored bytes, checked under the selected read policy |
| `read_logical_payload_into(&entry, &mut Vec<u8>)` | decompressed if the record is compressed |
| `decode_block_into::<T>(&entry, &mut scratch, &mut out)` | decode into `out` (reusing what it owns), reading the bytes through `scratch` |

All three are bound to this open handle's snapshot, not to a path. The entry is
yours and is therefore not trusted: an extent past the snapshot is refused.
Checksum mismatches are refused when the selected read policy verifies data CRCs.

### Reads resolved through a directory you supply

`with_directory(&directory)` returns the same reads answered against a directory
of your own instead of the handle's. A directory is anything implementing
`RecordDirectory` — in practice the `Vec<RecordIndexEntry>` that
`index_entries_into` or `open_readonly_with_scratch` filled, or a subslice of
it.

```rust
let mut index = Vec::new();
let file = VarlitFile::open_readonly_with_scratch(spec, path, &mut index)?;
let points = file.with_directory(&index)?.blocks::<Point>()?;
```

`open_readonly_without_directory(spec, path, &mut index)` opens a handle that
keeps **no** directory — nothing that scales with the file — and hands you the
one the scan produced. The `index` parameter is output, not scratch, and is not
optional: a handle that keeps no directory cannot produce one afterwards.

The reads are not withdrawn in that mode. They are answered through
`with_directory`; calling one on the handle itself returns
`Error::NoResidentDirectory { operation }`, which names the read and the way to
answer it, rather than answering as an empty file would.

### An index you build only as far as the question

`record_map(&mut buffer)` returns a `RecordMap<'_>` over a
`Vec<RecordIndexEntry>` you own. It **reads nothing** when it is created and
walks the record chain forward one record at a time, only when asked.

| Method | Meaning |
| --- | --- |
| `find(predicate)` | walk until a record matches, and stop there |
| `fill_to(k)` | walk until the map holds `k` entries, or the file ends |
| `fill()` | walk to the end — this is `index_entries_into`, spelled as the case it is |
| `advance()` | walk exactly one record |
| `entries()` / `len()` / `is_complete()` / `resume_offset()` | what has been walked |
| `clear()` | empty the buffer and rewind to the start of the file |
| `release()` | empty the buffer and keep the position |

```rust
let mut buffer = Vec::new();
let mut map = file.record_map(&mut buffer)?;          // no read yet
let hit = map.find(|entry| entry.block_id == Note::ID)?;
if let Some(entry) = hit {
    let mut note = Note::empty();
    file.decode_block_into(&entry, &mut payload, &mut note)?;
}
let points = file.with_directory(&map)?.blocks::<Point>()?;  // the prefix walked
map.clear();
```

It implements `RecordDirectory`, so a map **is** a directory and every read
`with_directory` serves is answered against the prefix walked so far. It borrows
the *buffer*, not the handle, so reads through the handle stay available while
the map is alive.

A second question resumes where the first stopped, and `find` searches what is
already framed before it reads anything, so asking twice costs the records
between the two answers. For an incremental directory pass, use `fill_to(k)`,
process the entries, then `release()` and repeat. This bounds retained directory
entries by the chosen batch; buffer capacity and fixed scratch add to that
storage. The framing cursor advances without rescanning the released prefix.
Payload reads are separate work.

**What a partial index does not check.** A full open validates sequence
uniqueness across every record in the file; a map validates nothing beyond the
prefix it has read. A duplicate sequence past the stopping point is not detected
until a walk reaches it. A map walked to completion (`fill()`) is the open
scan's index, entry for entry.

### Marking a record dead

Available on `VarlitFile` and `VarlitWriter` when the format declares
`liveness: footer_flags`. See the format-author guide for what the option costs.

| Call | Takes | What it does |
| --- | --- | --- |
| `mark_record_dead(record_offset)` | `&mut self` | Sets the dead flag in the record's footer — four bytes, in place. Not synced here; it rides the next `flush`/`commit`/`sync` |
| `record_is_dead(record_offset)` | `&self` | Frames that record and reads the flag |
| `RecordIndexEntry::is_dead()` | `&self` | The same answer from an entry you already hold, at no extra read |
| `opened_after_crash()` | `&self` | Whether the writer that held this file before this handle died without releasing it. `false` on a read-only handle by construction — answering needs the object lock a reader does not hold |

The mark is **advisory**: the record is still framed, still returned by every
collection, and still on every chain that pointed at it. A defragmenting rewrite
is what acts on it. Refused for an internal record (`ReservedBlockId`) and for a
format without the option (`InvalidFormatSpec`).

### Reclaiming the space a dead record holds

| Call | Takes | What it does |
| --- | --- | --- |
| `defragment()` | `&mut self` | Rewrites the file without the records marked dead and publishes it by atomic rename. Returns `DefragmentReport { records_dropped, bytes_before, bytes_after }` |

A reader open across it keeps its own generation whole — the old object is
unlinked but alive while a handle holds it — and moves with `is_current()` plus
`reopen_readonly()`. The cost of publishing that way is peak disk: both
generations exist at once.

Refused (`InvalidFormatSpec`) without `liveness: footer_flags`, for custom
physical layouts and for matrix storage; refused (`NoResidentDirectory`) on a
handle that keeps no directory.

<a id="an-open-that-reads-no-record"></a>

### Metadata-only open and explicit scans

`open_readonly_lazy(spec, path)` opens from the **open digest** at the end of
the file: the three facts an open needs that live in no single record — where
the committed file ends, what sequence the next append takes, and where each
block's newest record sits. `IndexPolicy::with_open_digest_on_flush(true)`
writes them at each commit point.

The handle keeps no directory. `blocks`, `scan` and `keyed_blocks` return
`Error::NoResidentDirectory` and are answered through `with_directory(&map)`;
`block_chain`, `block_tail_offset`, `read_block_at` and every entry-taking
`_into` read need no directory and work directly — which is what the block tails
are in the digest for.

Lazy open never starts a full scan after a metadata error. It uses the header
tail table when declared, otherwise the trailing digest. The selected source
must be usable; it does not silently switch sources after failure.

| API | On unusable summary metadata |
| --- | --- |
| `VarlitFile::open_readonly_lazy[_with_report]` | Return an error; no scan |
| `VarlitFile::open_lazy[_with_report]`, `VarlitWriter::open_lazy[_with_report]` | Return an error and release the writer lock; no scan or native-tail truncation |
| `VarlitFile::open_readonly_scan` | Explicitly reconstruct read-only state without retaining a directory |
| `VarlitFile::open_scan`, `VarlitWriter::open_scan` | Explicitly use normal scanning-writer recovery, including uncommitted-tail truncation |
| `*_lazy_or_scan[_with_report]` | Preserve the former automatic summary-to-scan behavior |

`Error::LazyOpenMetadataUnavailable { metadata }` identifies a missing, stale or
structurally invalid selected summary. This intentionally does not claim to
distinguish missing metadata from all possible corruption without scanning.
Specific I/O, allocation, checksum, schema and resource errors are propagated.
`LazyOpenVerificationRequiresScan` means the configured AtOpen payload
verification requires a scanning API. A strict successful report is
`LazyOpenSource::HeaderTails` or `Digest`; only the explicit automatic mode can
report `FullScan`. Scanning can also fail and returns its own error.

The `*_lazy_or_scan` APIs preserve legacy recovery: they try header tails, then
the trailing digest, then a scanning open. Except for the existing missing-limit
and trusted-unbounded authorization refusals, metadata errors can be replaced by
the scan result. Select strict lazy open when the original error must be retained.

```rust,no_run
# fn open(spec: varlit::FormatSpec, path: &std::path::Path) -> varlit::Result<varlit::VarlitFile> {
use varlit::{Error, VarlitFile};
match VarlitFile::open_readonly_lazy(spec, path) {
    Ok(reader) => Ok(reader),
    Err(Error::LazyOpenMetadataUnavailable { .. }) => {
        // Application policy explicitly permits reconstruction here.
        VarlitFile::open_readonly_scan(spec, path)
    }
    Err(error) => Err(error),
}
# }
```

The option requires `block_offset_chain` and composes with both
`segment_on_flush` and `checkpoint_on_flush` — the three answer different
questions. It is off by default and a file written without it is byte-identical
to one written before it existed.

Common `VarlitWriter` APIs:

| API | Meaning |
| --- | --- |
| `push(&block)` | append fixed/variable user block; refuses a keyed block on a `keyed_offset_chain` format |
| `push_keyed(&block)` | append a keyed user block and maintain the keyed offset chain |
| `delete::<T>(&key)` | append keyed tombstone; maintains the keyed offset chain |
| `push_op::<T>(&key, &op)` | append user-defined merge op |
| `write_metadata(key, bytes)` | append internal metadata record |
| `replace(index, &block)` | **the door**: picks one of the three safe routes below from the format spec, and returns the published sequence |
| `replace_block(index, &block)` | sequence-preserving copy-on-write replacement; encoded size may grow or shrink |
| `replace_fixed(index, &block)` | same-size copy-on-write replacement; already-open readers keep their snapshot; refused on `segment_on_flush` and on `open_digest_on_flush` |
| `unsafe replace_fixed_in_place_exclusive(index, &block)` | expert-only in-place replacement; same two refusals; caller must exclude readers and writers **and must not change a keyed record's key** |
| `replace_rewrite(index, &block)` | rewrite whole file through temp file |

`replace` takes no strategy argument, because which route is legal follows from
the declaration rather than from the caller:

| the format declares | `replace` calls |
| --- | --- |
| a record footer (an offset chain, or a commit marker) | `replace_block` |
| no footer, and `T` is a fixed block | `replace_fixed` |
| no footer, and `T` is variable | `replace_rewrite` |

It never picks the `unsafe` path. Call a named method instead when you want a
particular mechanism and would rather be refused than rerouted — each still
refuses where its format cannot serve it, and those refusals are the reason the
choice is not the caller's to make.

Both in-place routes are refused with `Error::InvalidFormatSpec` on a format
declaring `segment_on_flush` or `open_digest_on_flush`. They restamp a record
that an already written derived record describes, and they rewrite no derived
record — while taking a *fresh* sequence, which leaves a digest's stored
high-water mark below the file's true maximum. A lazy open seeds its writer from
that mark rather than recounting, so the next append would reuse a sequence and
the file would stop opening with
`InvalidCanonicalEncoding("duplicate native record sequence")`.

Neither refusal costs the capability. `open_digest_on_flush` forces
`block_offset_chain` on, so such a format is a record-footer format and `replace`
routes it to `replace_block`, which republishes and rebuilds every derived
record. Only a caller naming an in-place method directly meets the refusal.

`index: header_tails` is the one lazy route that is *not* refused, because its
region has a cold state that claims nothing and a digest record has no
equivalent. An in-place replacement resets the region instead: the next open
reports `LazyOpenSource::FullScan` and the next commit warms it again.

All four safe replacement entry points select their target by block id and refuse a
stored `block_version` that differs from `T::VERSION` with
`Error::BlockVersionMismatch` (F-01). `replace_rewrite` and
`unsafe replace_fixed_in_place_exclusive` previously did not: they copied the
stored version while substituting the new payload, which is a persistent
type/version disagreement on disk whenever `schema_hash = 0` opts out of
header-level schema locking. The check is no longer a step a path performs — it
is a precondition of resolving the target at all, so a replacement path added
later inherits it. The refusal now also precedes the size, limit, and
generation checks, so a call that is wrong in two ways reports
`BlockVersionMismatch` where `replace_fixed` previously reported
`ReplaceSizeMismatch`.

`unsafe replace_fixed_in_place_exclusive` additionally requires, for a keyed
block on a `keyed_offset_chain` format, that the replacement keep the record's
key. Nothing can check that at a `T: VarlitBlock` signature and there is no new
generation in which the physical chain could be rebuilt, so it is part of the
unsafe contract. What the method does guarantee unconditionally is that the
affected block's resident keyed-tail map is dropped *before* the first byte is
written (F-02), so no stale predecessor can be read afterwards even if the
write fails half-way or the contract is violated.
| `commit()` | write explicit transaction marker without an implied fsync |
| `commit_durable()` | write an explicit transaction marker with ordered flush/sync barriers |
| `flush()` | write buffered records/checkpoints/manifests/marker |
| `sync()` | request durable persistence |

Keyed appends on a `keyed_offset_chain` format must go through a maintaining
path. The generic `push` / `push_info` (on both `VarlitWriter` and `VarlitFile`)
cannot see `T::Key`, so it would write `prev_same_key_offset = None` and
truncate the chain; it therefore refuses a keyed block on such a format with
`Error::KeyedChainRequiresKeyedApi { block_id }`. The refusal is unconditional —
it fires on the first push, not only where a predecessor exists — so the
contract never depends on call order, and nothing is written. Use `push_keyed` /
`push_keyed_info` instead, which resolve the predecessor from the persisted
keyed tail offsets and rebuild that cache after reopen. `delete` maintains the
chain on its own. Unkeyed blocks and formats without `keyed_offset_chain` are
unaffected, and the generated typed keyed writers already take the maintaining
path.

Writer encoding is limit-bounded end to end: push, metadata, replacement, and
keyed-op entry points encode through a budgeted encoder, so an oversized value
fails with `Error::LimitExceeded { resource: "logical payload length" }` and
buffering stops at the limit instead of materializing the full encoding first.
Generated nested variable fields inherit the parent encoder's remaining budget
(`Encoder::encode_nested_to_vec`), and matrix cell writes bound the encode by
the slot stride while preserving the exact-size `MatrixSizeMismatch` error.
With `checkpoint_on_flush`, the flush-time checkpoint decision is O(1) writer
state, not an index scan, so flush-per-record workloads stay linear.

Common `VarlitReader` APIs:

| API | Meaning |
| --- | --- |
| `blocks::<T>()` | lazy typed records by block type |
| `keyed_blocks::<T>()` | latest put by key after tombstones are applied |
| `materialized_keyed_blocks::<T>()` | applies puts, ops, tombstones |
| `metadata(key)` | latest metadata value |
| `schema_manifest()` | latest embedded manifest if present |
| `scan()` | physical record event iterator |

### An editable region in the header

Every other way varlit records a fact is append-only, which is exactly wrong for
a fact that changes: a watermark, a processing state, a pointer into an external
system. `HeaderSlots` reserves a **fixed** run of bytes in the file header that
a caller rewrites in place for the life of the file.

```rust
varlit_format! {
    pub struct AppFormat {
        magic: b"MYFORMAT";
        version: 1;
        endian: little;
        schema_hash: computed;
        header_slots {
            capacity: 256;
            integrity: rolling;
            blocks: [Watermark];
        }
        blocks: [Sample, Watermark];
    }
}

let mut file = AppFormat::create("f.varlit")?;
file.write_header_block(&Watermark { label: "ingested".into(), at: 42 })?;
let mut mark = Watermark::empty();
assert!(file.read_header_block::<Watermark>(&mut mark)?);
assert_eq!(mark, watermark);
```

| method | takes | does |
| --- | --- | --- |
| `read_header_block::<T>(&mut out)` | `&self` | decodes `T` from the cached header into `out`; no I/O, `false` if never written |
| `write_header_block(&block)` | `&mut self` | writes it in place, replacing any earlier value for the same id |
| `remove_header_block::<T>()` | `&mut self` | removes it, freeing its bytes |
| `seal_header_slots()` | `&mut self` | fixes the checksum and refuses every later write |
| `header_slots_capacity()` | `&self` | the declared size; `0` when undeclared |
| `header_slots_used()` / `header_slots_free_bytes()` | `&self` | how much of it the current blocks occupy |
| `header_slots_sealed()` | `&self` | whether the seal is down |

**The size is fixed, and that is the point.** The append log starts after the
region, so a region that could grow would move every record in the file. Writing
into it changes no file length and invalidates no record offset — which is why
this is a reservation rather than an append.

**How large it may be: 64 KiB unless you declare otherwise.** The region shares
the file-header extension budget with every other header block, and that budget
is `ReadLimits::max_file_header_extension_len`, declared as `header_extension`
in a `limits { ... }` block:

```rust
limits {
    // ... the rest of the block ...
    header_extension: 524_288;
}
header_slots {
    capacity: 262_144;
    integrity: rolling;
    blocks: [Watermark];
}
```

A capacity over the budget is refused at create with
`Error::InvalidFormatSpec("header_slots capacity exceeds the file-header
extension limit")`, not silently clamped. The same ceiling bounds the **read**
path, which is what it exists for: the region's on-disk length field is a `u32`,
so without a ceiling a hostile header could name a 4 GiB region and have open
allocate it before a single block is parsed.

Because it is a `ReadLimits` field it is a property of one open and not of the
file — it changes no byte and is not folded into the schema hash. The practical
consequence: **a reader that does not declare the same ceiling refuses a file
whose region is over 64 KiB**, and it refuses it at open. That is the intended
behaviour for a reader hardening itself against untrusted input, and it is a
mistake waiting to happen if two crates declare the same format twice with
different `limits { }` blocks.

**A rewrite replaces rather than appends.** Writing the same block a thousand
times costs what writing it once costs, so a fixed region absorbs an unbounded
number of edits. Two *different* blocks must both fit; a write that would not is
refused with `Error::LimitExceeded` before any byte is written, and the region
is left exactly as it was.

**Choosing the integrity policy** — this is the "exempt from the CRC, or compute
it and freeze it" decision, and all three are declarations, not runtime calls:

| want | declare | costs |
| --- | --- | --- |
| the region editable forever, never verified | `integrity: none` | nothing; a changed byte is a changed value |
| every read verified against a checksum kept current | `integrity: rolling` | one pass over the region per write and per read |
| the value fixed at a point you choose, then immutable | `integrity: sealed` | the same, plus one `seal_header_slots()` call |

Under `sealed`, the checksum is advisory until the seal — the region is still
being edited, so a mismatch says nothing a caller could act on. After the seal,
reads verify with `Error::ChecksumMismatch` and writes fail. Nothing unseals a
region; a caller who could unseal would have gained nothing over `rolling`.
`seal_header_slots()` on any other policy is refused rather than ignored,
because `none` has no checksum to freeze and `rolling` recomputes on every
write.

The checksum is FNV-1a 32 and works without the `integrity` feature. Gating it
would have made `rolling` and `sealed` silently do nothing in a default build.

Undeclared, the region costs nothing: the header bytes and
`computed_schema_hash()` are the ones the format produced before it existed. It
combines with matrix blocks: a matrix's records start after the file header,
wherever it ends (`header_tails_and_header_slots_combine_with_a_matrix`).

## Scalable Stream Handles

The default `varlit_format!` expansion generates a stream handle family for
files whose open and append cost must not depend on total file bytes or record
count. It needs no Cargo feature.

| Generated API | Meaning |
| --- | --- |
| `create_stream_writer(path, options)` | create native file plus state-only `.vks` |
| `open_stream_writer(path, options)` | resume from `.vks` without a native prefix scan; a sidecar that trails the primary is recovered as `restore_stream_writer` does |
| `open_stream_reader(path, options)` | pinned open from `.vks` plus the synced records after it |
| `restore_stream_writer(path, options)` | return to the newest confirmed checkpoint, keep every synced record after it, cut the unsynced suffix |
| `bootstrap_stream_checkpoint(path, options)` | explicit native scan to create a missing `.vks` |
| `bootstrap_stream_checkpoint_with_progress(path, options, scan, observer)` | controlled bootstrap scan with progress/cancellation |
| `clear_stale_writer_lock(path, policy)` | O(1) explicit lock recovery without opening/scanning native data |

Generated `push_<blocks>(iterator, BatchOptions)` methods coalesce native
records into bounded writes and return one compact `BatchAppendInfo` rather
than a per-record vector. The stream writer also exposes `flush()`, `sync()`,
`immediate()`, `set_immediate_policy(policy)` and `compact_index()`. In a
`keyed_offset_chain` format the stream writer generates no push or delete
method for a keyed block, because it keeps no previous-key chain. Stream
readers expose `events()`, `verify_all()`, lazy typed plural methods for
explicit sequential scans, `follow()`, `follow_events`, `follow_blocks`,
`snapshot_status()` and `release_snapshot()`. They have no lookup by key; see
[bounded keyed storage](design/bounded-keyed-storage.md) for the design that
will provide one.

Stream readers also expose
`verify_all_with_progress(ScanOptions, observer)`. Scan control uses:

| Type | Meaning |
| --- | --- |
| `ScanCancellationToken` | cloneable cooperative cancellation flag |
| `ScanProgressOptions` | callback cadence by records and/or bytes |
| `ScanOptions` | cadence plus an optional borrowed cancellation token |
| `ScanProgress` | phase, completed records, scanned bytes, absolute current offset, snapshot length |
| `ScanProgressPhase` | `Started`, `Running`, or final pre-publication `Complete` |

Defaults notify every 16,384 records or 16 MiB. Cancellation returns
`Error::ScanCancelled { progress }`; cancellation observed through the final
callback/check publishes no new sidecar target. Progress bookkeeping does not
allocate per record.

Normal stream open never bootstraps, rebuilds, verifies or scans the native
prefix. A missing, stale or identity-mismatched `.vks` is an error.
`open_stream_writer` recovers one condition itself: a sidecar that trails the
primary (an unconfirmed tail or an interrupted generation) is restored to its
newest confirmed checkpoint, the verified sync-marker prefix after it is kept
and replayed, and the unsynced suffix is cut. `sync()`, not `flush()`, makes
records durable and visible to readers.

### One Poison Flag Per Writer

A stream writer poisons itself when an append fails and the truncate-or-seek
rollback that should undo it fails too: the file's tail is then in an unknown
state and no further mutation is safe. Every later mutation is refused with
`Error::WriterPoisoned`. A sidecar publication whose outcome is uncertain also
poisons the writer.

The writer has a single poison flag, and its mutating entry points require a
witness value that only the poison check produces — a caller that skips the
check does not compile. Each published chunk of a batch takes its own fresh
check, so a chunk that poisons the writer stops the chunks after it in the
same call.

`Error::DiskIndex` contains the original boxed `DiskIndexError`; callers should
match that source rather than parse display text. Sidecar lock contention maps
to `Error::IndexBusy`. Bootstrap refuses an existing `.vks`, so it cannot bless
an unsynced tail; to replace a `.vks` known to be stale, remove it explicitly
and bootstrap again.

Sidecar publication reports durability honestly: stream sidecar create and
stream bootstrap can return `Error::PublishedButParentSyncPending` when the
sidecar was atomically published but the parent-directory sync failed. The
published sidecar is preserved and usable; treat the error as a durability
warning, not as "nothing was published". `verify_all()` is the whole-file
integrity scan.

Typed access requires an immutable identity for the block in `FormatSpec`.
An absent identity returns `Error::InvalidFormatSpec` before codec invocation.

Stream primaries carry a per-create 128-bit nonce as their first record
(reserved block id `CREATION_NONCE_BLOCK_ID`, one record at create and nothing
per append), and the sidecar binds to it through its recorded
primary-generation witness. Replacing a primary in place with another
equal-length primary of the same format is refused at open with
`DiskIndexError::PrimaryGenerationMismatch` on both the reader and the writer;
the recovery is to remove the `.vks` and run `bootstrap_stream_checkpoint`. A
primary bootstrapped from a resident `VarlitFile` carries no nonce and is
protected only by the weaker content-window witness over its leading bytes.

Manual `VarlitBlock` implementations must state
`IS_KEYED` explicitly. This is a compile-time chain-safety requirement; macro
generated blocks already provide the exact value.

See [Scalable Stream I/O](scalable-io.md) for declarations, durability
ordering and checked extent types.

`scan()` yields entries whose physical payload can be read through
`RecordIndexEntry`. Use `read_payload_limited(path, physical_limit)` for a
stored-byte ceiling and
`read_logical_payload_limited(spec, path, physical_limit, logical_limit)` to
bound both the stored allocation and decoded logical allocation.
All path-taking `RecordIndexEntry` helpers are deliberately low-level and
non-snapshot: they reopen whatever object the pathname currently names. Their
default forms resolve finite standard/spec limits; `*_limited` lets tooling
supply stricter or deliberately raised ceilings after validating provenance. Generated
readers and typed collections do not use them; they retain the originally
opened object and captured logical EOF.
`checked_physical_end()` is the overflow-reporting extent API; `physical_end()`
remains a saturating compatibility helper.

Append sequence numbers start at `0`, including after reopening an empty file.
After `u64::MAX` is published once, further append operations return
`SequenceExhausted` before mutating the file. A returned append or streamed
layout write error is rolled back to the prior EOF when possible. If rollback
fails, the handle returns `WriteRollbackFailed`, becomes poisoned, and rejects
later mutation, `flush`, and `sync` with `WriterPoisoned`.

Copy-on-write replacement has distinct post-publication failure states.
`PublishedButRebindFailed { sequence, source, parent_sync }` means the new
generation was already atomically published, but the current writer could not
reopen and bind to it. Discard the poisoned writer and reopen the path to
inspect the published state. Do not blindly retry the same logical update:
publication may already have applied it. `parent_sync: Some(_)` adds the second
durability fact for a double fault (F-07): the parent-directory sync for the
published pathname failed too, so the rename is not yet proved durable against
power loss. `None` means only the first two facts. Downstream code that
destructured this variant with `{ sequence, source }` must add `parent_sync` or
`..`. On Windows, `ReplacePublicationIndeterminate` means
`ReplaceFileW` failed with error 1176/1177 and reconciliation could not prove
the outcome: the pathname state is unknown, the replacement temp file is
preserved, and the writer is poisoned. Inspect the pathname and the preserved
temp separately; `classify_replace_publication_error(raw_os_error)` exposes
the underlying classification (1175 = replaced file intact, 1176/1177 =
indeterminate, everything else = pre-publication).

`varlit::Error` is `#[non_exhaustive]`. Downstream exhaustive matches must keep a
wildcard arm so new diagnostics can be added without another enum-shape break.

## Keyed Storage

A format that declares keyed storage keeps one record per key, updated in
place, so the file follows its live data rather than the number of updates.
See [`design/bounded-keyed-storage.md`](design/bounded-keyed-storage.md) and,
for the layout, [`spec.md`](spec.md#keyed-storage-wire-contract).

```rust,ignore
varlit_format! {
    pub format Profiles {
        magic: b"PROFILES";
        version: 1;
        schema_hash: computed;
        integrity: crc32;
        keyed_storage: in_place(free_space = 3%); // 1% to 5%; `in_place` alone is 5%
        blocks {
            variable Profile(id = 1, key = [user]) { user: String, bio: String, }
            fixed Visit(id = 2) { at: u64, }
        }
    }
}

let mut writer = Profiles::create_writer(&path)?;
writer.put_profile(&profile)?;                 // insert or replace, in place
writer.remove_profile(&"ada".to_string())?;    // true when it was there
writer.push_visit(&Visit { at: 1 })?;          // unkeyed blocks still append
writer.sync()?;                                // publishes one generation

let reader = Profiles::open_reader(&path)?;
let mut scratch = Vec::new();
let mut out = Profile::empty();
if reader.get_profile(&"bob".to_string(), &mut scratch, &mut out)? { /* ... */ }
```

- **Generated methods.** For each keyed block of such a format the writer has
  `put_<block>`, `remove_<block>` and `get_<block>`, the reader `get_<block>`.
  No `push_`, `delete_`, `replace_` or listing methods are generated for those
  blocks. The untyped equivalents are `VarlitFile::put_keyed`,
  `remove_keyed` and `get_keyed` (also on `VarlitWriter` and `VarlitReader`).
- **Reads.** `get_keyed(key, scratch, out)` encodes the key into `scratch`,
  then reads the payload into it and decodes into `out`; it allocates nothing
  else. A lookup reads a bucket page and the record: usually two reads.
- **Visibility.** A writer sees its own puts and removes at once. Readers see
  them after `sync`, which publishes them as one generation through the
  file's write-ahead ring; a writer dropped without `sync` publishes nothing.
  A reader keeps its generation until `follow`; one the ring has moved past
  gets `MatrixSnapshotExpired` and must `follow`.
- **Space.** An update that fits is written where the old value was; one that
  does not moves, and the old place is reused. Deletes leave no tombstone.
  When free space exceeds the declared ratio of live bytes the writer moves
  records out of the sparsest regions. Beyond the live data a file holds
  whole-region slack (1 MiB regions), the ring (`KeyedStorage::ring_bytes`,
  16 MiB by default) and room to stage the largest publication atomically;
  none of these grows with the number of updates.
- **Refusals.** In a keyed-storage format, appending a keyed block to the log
  (`push_info`, `push_keyed`, `delete`) fails with
  `Error::KeyedStorageRequiresPut`; keyed calls on a format without keyed
  storage fail with `Error::InvalidFormatSpec`.
- **Stats.** `VarlitFile::keyed_storage_stats()` returns `KeyedStorageStats`:
  the generation shown, keys, live bytes, heap bytes, free region bytes and
  slot buckets.

## Collections

| Type | Meaning |
| --- | --- |
| `BlockVec<T>` | lazy typed collection for one block type |
| `BlockVec::len()` | number of visible records |
| `BlockVec::get(index)` | decode one item on demand |
| `BlockVec::iter()` | lazy iterator of decoded items |
| `KeyedBlockVec<K, T>` | latest visible put per key after tombstones |
| `KeyedBlockVec::get(&key)` | decode latest value for key |
| `KeyedBlockVec::keys()` | iterate known keys |

`BlockVec` and `KeyedBlockVec` read payload bytes lazily from the captured file
snapshot, even if the pathname is later replaced.

## Blocks And Codecs

| Trait | Implemented by |
| --- | --- |
| `VarlitBlock` | all stored block types |
| `VarlitKeyedBlock` | blocks with a stable key |
| `VarlitMerge` | keyed blocks with user-defined ops |
| `VarlitMatrixBlock` | matrix cell blocks |
| `VarlitEncode` / `VarlitDecode` | field and block payload codecs |
| `VarlitMigration<From, To>` | explicit semantic migration |

Most users get these implementations from `varlit_format!` or
`#[derive(VarlitBlock)]`. Implement codecs manually only for domain-specific
value types.

Canonical booleans are encoded only as `0` or `1`; other bytes are rejected.
Decoded `BTreeMap` and `HashMap` values reject duplicate keys instead of silently
keeping one value. `HashMap` encoding requires `Ord` for deterministic key order
but no longer requires `Clone`.

`VarlitBlock` requires a `const SCHEMA_FINGERPRINT: u64` with no default.
`#[derive(VarlitBlock)]` computes it deterministically (FNV-1a 64 over the
canonical schema: id, version, kind, endian, keyedness, ordered field
name/type identities, and each field's resolved codec `SCHEMA_ID`).

Typed registration validates against the format, not against whichever type
arrived first. When the `FormatSpec` declares an identity for the block id — as
every `varlit_format!`-generated spec does — that immutable
`(endian, keyedness, fingerprint)` triple is the authority, and a `T` that
disagrees is rejected with `Error::BlockSchemaFingerprintMismatch`,
`Error::BlockKeyednessMismatch`, or `Error::EndianMismatch` before anything is
cached or written. Call order therefore cannot decide which wire type a process
accepts.

Endian is compared *after resolution*, not as an opaque tag. Every typed path
resolves a block's byte order as `T::ENDIAN.unwrap_or(spec.endian)`, so the check
compares `registered.unwrap_or(spec.endian)` with `T::ENDIAN.unwrap_or(spec.endian)`.
A declared override that contradicts the format's own declaration is rejected —
that is the silent byte swap this exists to catch — while two declarations that
resolve to the same byte order are accepted, because they genuinely produce
identical bytes.

Every typed block contract is validated directly against the spec's immutable
`&'static` identity table. There is no first-use registration or process-global
cache. Generated formats supply the table automatically. Manual formats attach
it with `.with_block_identities(&[MyBlock::IDENTITY, OtherBlock::IDENTITY])`;
import `VarlitBlock` to use these associated constants. Missing entries return
`Error::InvalidFormatSpec` before the codec runs. Raw record access does not
require a typed identity. A manual implementation mirroring a generated block
must reuse its schema fingerprint.

Keyedness is an invariant, not a free-form flag. A type that implements
`VarlitKeyedBlock` must declare `VarlitBlock::IS_KEYED = true`; every public
keyed generic entry point — keyed collections, stream push/delete, merge and
compact, low-level `VarlitFile`/reader/writer deletes, `keyed_blocks`,
`key_tail_offsets`, and the self-test keyed case — evaluates `KeyedBlockContract::<T>::OK`, turning an
`impl VarlitKeyedBlock` with `IS_KEYED = false` into a post-monomorphization
compile error. First-seen registration remains the runtime backstop and also
rejects a keyedness disagreement for the same block id
(`Error::BlockKeyednessMismatch`). Manual
implementations must state `IS_KEYED` explicitly (no chain-unsafe default).

## Matrix API

Matrix API exists on `VarlitFile`, `VarlitReader`, `VarlitWriter`, and generated
typed wrappers.

**Matrix reads take `&self`.** `read_matrix_cell`, `matrix_cell_payload`,
`read_matrix_aux`, `matrix_aux_len` and `matrix_cell_status` borrow their handle
by shared reference. Writes and `follow()` still require `&mut self`.
`copy_matrix_cell_bytes_from` also borrows its source handle by shared reference.
A shared borrow does not make the handle safe to share between threads.

Reader handles are `Send + !Sync`: each reading thread owns a reader and its
private caches, and creates its cursors there. Each query reads the reader's
snapshot; `follow(&mut self)` adopts later publications explicitly. A reader
does not hold back the writer: one that falls further behind than the
write-ahead ring holds gets `MatrixSnapshotExpired` instead of a mixed answer.

`crates/varlit/tests/matrix_concurrent_reads.rs` checks the shared-borrow entry
points, `Send` handle bounds and correct cell values through independent readers.
`report_the_scaling_of_independent_handles` reports a timing comparison without
asserting a speedup. The ignored
`independent_reader_scaling_beats_one_thread_on_an_idle_host` is a manual
benchmark, not a CI performance guarantee. Historical shared-handle throughput
figures do not describe the current ownership model.

Matrix files written with the removed `VMAT` layout are refused. `matrix_generation()` returns the number of the published generation
the handle shows; `follow()` returns record bytes, so a matrix-only
publication returns zero from it. A tiled matrix keeps its write-ahead ring in
the primary file, so `matrix_generation_path()` returns the primary path; there
is no companion file. `FormatSpec::with_matrix_ring_bytes(bytes)` sets the ring
size of new matrix files. `compact_matrix()` was removed in 0.13.0: overwrites
do not grow the file, so there is nothing to reclaim. See [durability](durability-model.md#matrix-storage-and-publication).

| API | Meaning |
| --- | --- |
| `create_writer_with_dims(path, dims)` | create matrix file with runtime dimensions |
| `write_matrix_cell::<T>(key, &value)` | encode and write a slot, clearing commit first |
| `commit_matrix_cell::<T>(key)` | mark a written cell visible |
| `read_matrix_cell::<T>(key, &mut out)` | decode committed cell into `out` or return `MatrixNotCommitted` |
| `read_matrix_run_into::<T>(start, out)` | read consecutive cells into a caller-owned typed slice |
| `read_matrix_run_bytes_into::<T>(start, out)` | read verified encoded cells into a byte slice, length divisible by slot stride |
| `read_matrix_row_into::<T>(first_axis, out)` | read one complete storage row; slice length equals dimension 1 |
| `read_matrix_column_into::<T>(start, out)` | hold dimension 1 fixed and read `out.len()` cells along dimension 0 |
| `read_matrix_column_bytes_into::<T>(start, out)` | same selected cells packed into encoded bytes; length divisible by slot stride |
| `matrix_cell_status::<T>(key)` | committed/not committed status |
| `clear_matrix_cell::<T>(key)` | clear one cell commit bit |
| `clear_matrix_category(category)` | clear a whole commit category |
| `matrix_cell_payload::<T>(key)` | checked raw payload bytes for a committed cell |
| `write_matrix_aux(name, offset, bytes)` | write preallocated noncommit aux bytes |
| `read_matrix_aux(name, offset, len)` | read aux bytes |
| `read_matrix_region_into::<T>(region, out)` / `read_matrix_region_from::<T>(cursor, out)` | read a rectangle row-major into `out`; returns how many cells were filled and a cursor to continue from |
| `read_matrix_cells_into::<T>(keys, out)` | read the cells `keys` names, in any order and with repeats, into `out` in key order |
| `matrix_resume_signal(category)` | `Clean` when none or all cells of the category are committed, `Partial` otherwise |
| `verify_matrix_metadata()` | check every existing tile's header and commit bitmap now and report damaged tiles |
| `rebuild_matrix_commit_from_crc::<T>()` | rewrite the block's commit bits: committed exactly where a written slot's CRC32 matches |
| `follow()` | adopt the newest published generation (read-only handles) |
| `write_matrix_cell_durable` | write/commit with ordered durability barrier, publish, then a post-commit hook |

Matrix run and column reads take `&self` on `VarlitFile`, `VarlitReader` and
`VarlitWriter`. Format-first readers also expose `cell_run_into`,
`cell_run_bytes_into`, `cell_row_into`, `cell_column_into`, and
`cell_column_bytes_into` for a block named `Cell`, including
through the generated read trait.

**Select the contiguous direction in the schema.** `dims = [row, col]` stores
adjacent columns together; a run advances through those columns and then to the
next row. `dims = [col, row]` stores adjacent rows together within each column.
Declare the commit keyspace in the same order. Generated keys retain the named
fields; low-level `MatrixKey::new(first, second)` follows declaration order.
The growing dimension must remain the first axis for every matrix block.
Use `read_matrix_column_into(start, out)` for the opposite axis. It returns
`(start.first + i, start.second)` in declaration order: a logical column with
`dims = [row, col]`, or a logical row with `dims = [col, row]`. Its length is chosen
by the caller, including for growing files. This provides a bulk API over strided
storage; it does not change the physical layout.

For example, with a row-oriented block and 128 columns:

```ignore
let mut row = vec![Cell { value: 0 }; 128];
reader.read_matrix_row_into::<Cell>(42, &mut row)?;
// Several rows, or a partial row followed by later rows, use a run:
let mut cells = vec![Cell { value: 0 }; 4096];
reader.read_matrix_run_into::<Cell>(MatrixKey::new(42, 16), &mut cells)?;
// Hold column 16 fixed and read 256 successive rows starting at row 42:
let mut column = vec![Cell { value: 0 }; 256];
reader.read_matrix_column_into::<Cell>(MatrixKey::new(42, 16), &mut column)?;
```

A column read reads each selected cell the way `read_matrix_cell` does: its
tile's commit bitmap, then its slot. Only selected cells must be committed;
missing cells in the gaps do not cause `MatrixNotCommitted`. Typed column reads
decode in bounded batches. A column crosses tiles, so it reads one bitmap and
one slot per cell; choose schema order for the primary access direction. A
column call does not retain a transposed copy of the matrix.

Typed column reads budget the caller's output, at most 64 KiB of payload scratch
(or one larger cell), cumulative decode allocations, and compression workspace.
Encoded column reads budget output and compression workspace. Metadata scratch
is fixed-size. On an error discard the output, which may already contain a prefix.
Like run reads, column reads take `&self` and stay on the captured generation;
only explicit `follow` advances a reader.

A run crosses row boundaries internally. Within a row it reads each tile's
row segment with one bitmap read and one slot read, rather than calling
`read_matrix_cell` once per cell. A typed batch is at most 1,024 cells and
64 KiB of payload, except that a wider cell requires one slot-sized buffer.
The supplied output is charged to `MaterializedBytes`; typed reads also charge
one payload buffer and cumulative decoder allocations. Fixed metadata scratch
is additional. To process a large matrix under a small budget, reuse a small
output slice across runs. Matrix blocks cannot declare compression.

Every requested cell must be committed. Holes return `MatrixNotCommitted`,
checksum damage is refused, and readers use their existing captured generation.
A failed call may have changed an output prefix: discard that result. Empty
runs validate the registered type and read no cells. Encoded output uses the
file/type endianness, not a cast to native structs. These APIs add no implicit
sync, reader sharing or locks, and do not alter the file format. They read one
matrix block at a time; cross-block joins remain application work over batches.

`rebuild_matrix_commit_from_crc::<T>()` visits every tile of `T`'s block that
exists and rewrites its commit bitmap whole: a cell is committed exactly when
its slot was written and the stored CRC32 matches the payload. A cell written
but never committed, or cleared with `clear_matrix_cell`, keeps its bytes and is
committed by the rebuild. It does not read the old bitmap, so it works on a
damaged one, as `clear_matrix_category` does. `MatrixCommitQuarantined`,
`MatrixFatalCorruption` and `FormatSpec::with_matrix_fatal_forensics()` were
removed in 0.13.0. See
[recovery](recovery-model.md).

The hook of `write_matrix_cell_durable` runs **after** the cell is committed
and its generation published, so it cannot un-commit anything. A hook failure is reported as the
typed published outcome `Error::MatrixCommittedButHookFailed { event, source }`
(the event is boxed so `Error` stays small) rather than a bare error, so a caller can tell "not committed, retry the write"
from "committed, retry only the notification"; the carried `MatrixCommitEvent`
is the one the hook was given. Retrying the whole call after that error repeats
the durable write and re-runs the hook.

A failed publication is typed too. `Error::MatrixPublicationUncertain { event,
source }` means the handle's generation had not advanced when the error came:
the frames may or may not be on disk, and the next writer open decides whether
the cell is committed. `Error::MatrixCommittedButDurabilityUnproven { event,
source }` means the generation had advanced and a later step of `sync()`
failed. Both poison the writer and skip the hook. A barrier callback's own
error is returned unchanged, poisons the writer, and means nothing was
published. See [durability](durability-model.md#ordered-barrier-policy).

The helper reserves its event and error boxes before mutation with `try_box`.
The error box is reserved before the cell is written and the event box before it
is committed; refusal of either returns before the commit bit is set. A later source
error moves into the reserved box, so the existing post-commit variants require
no additional allocation. Successful durable cell writes therefore include two
small allocations. Record/index/replacement paths instead retain inline
`AllocationFailureContext` facts if detailed error storage is refused; do not
repeat an already completed append. See `docs/durability-model.md` under
"Allocator Failure And Published Outcomes" for recovery semantics and scope.

Matrix slot types must have a width fixed by the type itself, because a slot
needs a stride the compiler knows. Scalars and fixed arrays of them qualify.
`PackedBitmap` does **not**: it owns a `Vec<u8>` and encodes a `bit_len` plus a
variable byte string, so it is rejected as a matrix field — it remains a normal
variable-field codec with a stable non-zero `SCHEMA_ID`.

Classification is decided **entirely by the resolved type, never by the source
spelling**. The generated `SLOT_STRIDE` folds each element's
`VarlitEncode::WIRE_TYPE` (not `size_of`), and a wire type with no fixed width
fails const evaluation with `matrix fields must have a width fixed by their
type; this codec does not`. Two consequences, both intended:

- a **type alias** of a supported scalar works. `type Word = u32;` and
  `window: [Word; 4]` are ordinary 4-byte and 16-byte slots, because the alias
  resolves before `WIRE_TYPE` is read;
- a user type merely *named* `u32` or `PackedBitmap` is **not** admitted. It has
  no `VarlitEncode` impl (or one without a fixed width), so it fails at the same
  const check or at the trait bound.

The macro's own syntactic check is deliberately permissive: it rejects only
source shapes that can never denote a fixed-stride type at all — references, raw
pointers, slices, tuples, trait objects, `impl Trait`, function pointers — and
defers everything else to `SLOT_STRIDE`. It is a diagnostic aid, not the
authority.

Lower-level matrix calls use `MatrixKey { scan, ch }`. Generated format-first
wrappers expose block-specific key structs such as `CellKey { scan, ch }` and
convert them into the runtime key internally.

Matrix cell read, write, and mmap access enforce the same typed registration
gate as the append-log APIs before any structural checks: a manual matrix
block that matches a registered block's id, shape, and stride but declares a
different `SCHEMA_FINGERPRINT` or keyedness is rejected with
`BlockSchemaFingerprintMismatch` / `BlockKeyednessMismatch` instead of
decoding foreign cells.

A damaged tile commit bitmap is not quarantined. Reads, writes, commits and
clears of that tile's cells, and the category's `matrix_resume_signal`, fail
with `InvalidMatrixLayout`; cells of other tiles stay readable.
`clear_matrix_category` resets the damaged bitmaps, and
`rebuild_matrix_commit_from_crc::<T>()` can follow in the same writer session
(`damage_in_place_is_reported_and_a_category_clear_recovers`). See
[recovery](recovery-model.md).

An overwrite stages the new slot in the writer; published bytes change only
through the write-ahead ring and its checkpoints, so readers keep their
snapshot until they follow or the ring passes them (`MatrixSnapshotExpired`).
An I/O error during a matrix mutation poisons the writer. `sync()` and explicit
Immediate publish the staged changes; `flush()` or dropping the writer does
not. Readers may overlap writes to the same cells, using a separate reader per
thread. See [durability](durability-model.md#matrix-storage-and-publication).

## Physical Layout API

Use this when the file bytes are not the Varlit-native container. The first
supported shape is an optional declared file header followed by an append stream
of declared segments with lead-in fields, caller metadata bytes, contiguous raw
bytes, optional footer fields, and finalized/backpatched offsets.

Physical layout declarations are field groups, not logical `VarlitBlock`
payload structs. `VarlitBlock` remains the native append-log payload unit;
`file_header`, segment `lead_in`, and `footer` describe bytes that sit around
metadata and raw regions.

| API | Meaning |
| --- | --- |
| `preset: varlit_native` | explicit native container preset; also the default |
| `preset: none` | custom layout owns byte zero |
| `layout { file_header ... segment ... }` | declare file header, segment lead-in, metadata region, raw region, optional footer |
| `Format::create_layout_writer(path)` | create a generated `FormatLayoutWriter` for format-first layout declarations |
| `Format::create_layout_writer_with_header(path, fields)` | low-level header-field creation path |
| `Format::create_layout_writer_with_typed_header(path, header)` | generated typed header-field creation path when a `file_header` is declared |
| `Format::open_layout_writer(path)` | validate an existing custom-layout file and append more segments through `FormatLayoutWriter` |
| `Format::open_layout_reader(path)` | open a generated `FormatLayoutReader` |
| `Format::inspect_layout_file(path)` | inspect native or custom physical file framing through one result type |
| `Format::inspect_layout_file_report(path)` | inspect complete physical prefix plus terminal tail status without claiming strict open success |
| `FormatLayoutWriter::write_data_segment(...)` | generated segment writer from the declared segment name |
| `FormatLayoutWriter::write_data_segment_streamed(...)` | generated segment writer that streams metadata/raw bytes into the file |
| `FormatLayoutReader::data_segments()` | generated typed segment info collection |
| `FormatLayoutReader::file_header()` | generated typed file-header info when a `file_header` is declared |
| `FormatLayoutReader::read_data_segment_metadata(index)` | generated metadata reader for the declared segment |
| `FormatLayoutReader::read_data_segment_metadata_range(index, offset, len)` | generated bounded metadata subrange reader |
| `FormatLayoutReader::read_data_segment_raw(index)` | generated raw-region reader for the declared segment |
| `FormatLayoutReader::read_data_segment_raw_range(index, offset, len)` | generated bounded raw-region subrange reader |
| `LayoutWriter::write_segment(SegmentWrite)` | write lead-in, metadata, raw bytes, footer, then backpatch offsets |
| `LayoutWriter::write_segment_streamed(SegmentWriteStream)` | stream metadata and raw bytes while still counting/backpatching offsets |
| `SegmentWrite::fields` | caller values for lead-in fields |
| `SegmentWrite::footer_fields` | caller values for footer fields |
| `SegmentWriteStream::write_metadata` | closure that writes metadata bytes directly to the file |
| `SegmentWriteStream::write_raw` | closure that writes raw-region bytes directly to the file |
| `LayoutReader::file_header_len()` | validated header length before the first segment |
| `LayoutReader::file_header_fields()` | validated declared file-header values |
| `LayoutReader::file_header_field(name)` | read one validated file-header value |
| `LayoutReader::segment_count()` | how many segments the file holds |
| `LayoutReader::segment(index)` | one validated physical segment range, by value |
| `LayoutReader::segments()` | every segment in order, `Iterator<Item = Result<LayoutSegmentInfo>>` |
| `LayoutReader::segments_into(&mut Vec<..>)` | the same into a buffer you own |
| `LayoutSegmentInfo::field(name)` | read validated lead-in values such as ToC mask or version |
| `LayoutSegmentInfo::footer_field(name)` | read validated footer values |
| `LayoutReader::read_metadata(index)` | read opaque metadata bytes for a segment |
| `LayoutReader::read_metadata_range(index, offset, len)` | read a checked metadata byte range without loading the whole region |
| `LayoutReader::read_raw(index)` | read contiguous raw-region bytes for a segment |
| `LayoutReader::read_raw_range(index, offset, len)` | read a checked raw byte range without loading the whole region |
| `LayoutScanReport` | tolerant scan result with complete segments and optional `LayoutTailInfo` |
| `Format::open_layout_reader_with_limits(path, limits)` | open the typed physical reader with runtime tightening |
| `Format::open_layout_reader_trusted_unbounded(path)` | explicit trusted physical-reader boundary |
| `Format::open_layout_writer_with_limits(path, limits)` | open the typed physical writer with runtime tightening |
| `Format::inspect_layout_file_report_with_limits(path, limits)` | bounded tolerant physical scan |

Layout readers retain the opened object and captured length. Whole/range reads
use positional I/O on that object, so replacing the pathname after open does
not redirect the reader. `LayoutReader::spec()` returns a sanitized ordinary
spec; private trusted authorization is never exported from a handle.

Stream callbacks are prevalidated before the first file mutation. A returned
callback error rolls back the partial segment when possible. A callback panic is
not caught: unwinding leaves the writer poisoned so it cannot be reused.
| `LayoutTailInfo` | terminal custom-layout scan status for truncated, invalid, or unmatched tails |

This path is separate from `create_writer/open_reader`; native append-log APIs
still write the `VARVE1/2/3` container. TDMS-style files use `preset: none` so
the first bytes can be `TDSm`.

Example:

```rust
varlit_format! {
    pub format FramedFormat {
        magic: b"FRAM";
        version: 1;
        limits {
            scan_bytes: 1_073_741_824;
            segments: 1_000_000;
            index_bytes: 134_217_728;
            record_payload: 67_108_864;
        }
        schema_hash: computed;
        preset: none;

        layout {
            file_header Header {
                bytes signature = b"VRV!";
                u16 header_version = 1;
            }

            segment DataSegment repeat until_eof {
                lead_in LeadIn {
                    bytes tag = b"SEGM";
                    u32 toc_mask;
                    i64 next_segment_offset =
                        finalize(target = segment_end, relative_to = after_lead_in);
                    i64 raw_data_offset =
                        finalize(target = raw_region_start, relative_to = after_lead_in);
                }
                metadata Metadata;
                raw_region Raw;
                footer Footer {
                    bytes write = b"END!";
                    u64 segment_len =
                        finalize(target = segment_end, relative_to = segment_start);
                }
            }
        }
    }
}
```

The macro also generates a typed physical-layout facade. For the example above,
the generated method names are derived from `DataSegment`:

```rust
let mut writer = FramedFormat::create_layout_writer("data.frame")?;
writer.write_data_segment(FramedFormatDataSegmentLayoutWrite {
    fields: FramedFormatDataSegmentLayoutFields { toc_mask: 0x1108 },
    footer_fields: FramedFormatDataSegmentLayoutFooterFields,
    metadata: b"objects",
    raw: b"raw channel bytes",
})?;
writer.flush()?;

let reader = FramedFormat::open_layout_reader("data.frame")?;
let header = reader.file_header();
assert_eq!(header.header_version()?, 1);
let first = reader.data_segment(0)?.unwrap();
assert_eq!(first.toc_mask()?, 0x1108);
let raw_slice = reader.read_data_segment_raw_range(0, 8, 8)?;
```

The generated wrapper still exposes the low-level `write_segment`,
`write_segment_streamed`, `read_metadata`, `read_metadata_range`, `read_raw`,
`read_raw_range`, and `segments` methods for adapters that need to bridge
dynamic metadata. Multiple segment descriptors are dispatched by their leading
literal lead-in prefix. A byte tag such as `bytes tag = b"DATA"` is the common
discriminator; immediately following literal numeric fields extend the dispatch
key. Generated segment-specific `index` parameters are per segment kind, while
the low-level range methods continue to use the global physical stream index.

`crates/varlit/examples/tdms_physical/common.rs` is a thin façade over the TDMS
example modules: `common/adapter.rs` contains the physical layout and
reader/writer adapter, `common/example_data.rs` contains fixture/scenario data,
and `common/verify.rs` contains self-verification. The combined
`tdms_physical_adapter` example exposes `write`, `append`, `read`, `read-bytes`,
and `inspect` subcommands, while the older `tdms_physical_writer` and
`tdms_physical_reader` examples are thin wrappers over the same shared
declaration. The adapter-authored file includes mixed TDMS raw channel objects
across three segments, a changed raw-data-index segment, TDMS-style
`same-as-previous` raw index reuse, bool/string/integer/float tagged
properties, and an adapter-owned `.vtidx` sidecar. The npTDMS harness verifies
exact channel values for signed/unsigned integer widths, single/double floats,
single/double floats with unit type ids, booleans, strings, timestamps, and
complex single/double floats. The reverse harness writes the scalar/channel
types that npTDMS can author correctly, parses them with the Varlit-based
adapter, appends a segment with Varlit, then verifies the result with both
npTDMS and the same adapter code. These examples are not a Varlit-provided TDMS
reader/writer feature.

`crates/varlit/examples/bmp_physical.rs` is the non-TDMS external-format smoke
example. `scripts/verify_bmp_with_pillow.py` writes a 24-bit BMP through
Varlit's generated layout writer and verifies it with Pillow, then writes a BMP
with Pillow and verifies the physical header and pixel payload through Varlit's
layout reader. The BMP example builds row-level chunk-index entries so padded
physical rows are represented as logical pixel chunks.

## Adapter Toolkit API

Use this layer when a custom physical layout has format-specific metadata or raw
region semantics. The toolkit is generic: TDMS, BMP, and future external
formats provide their own domain codecs and models.

| API | Meaning |
| --- | --- |
| `BinaryCursor::new(bytes, endian)` | checked endian-aware metadata/raw byte reader |
| `BinaryCursor::with_materialization_limit(bytes, endian, limit)` | cursor with an explicit cumulative ceiling for all owned arrays/strings |
| `Decoder::decode_from_slice_limited(bytes, endian, limit)` | canonical decode with a cumulative nested allocation budget |
| `BinaryWriter::new(endian)` | endian-aware byte builder for external metadata/raw payloads |
| `LengthPrefix` | implemented for `u8`, `u16`, `u32`, and `u64` length-prefixed values |
| `cursor.len_prefixed_bytes::<u32>()` | read a length-prefixed byte slice |
| `writer.len_prefixed_string::<u32>(value)` | write a length-prefixed UTF-8 string |
| `TaggedValueCodec` | user-owned mapping from external type ids to value enums |
| `ChunkIndexBuilder` | validate logical raw chunks against `LayoutSegmentInfo::raw_len` |
| `ChunkIndex::entries_for(key)` | inspect logical chunks for a stream/key |
| `SegmentReducer` | user-owned stateful reducer for segmented metadata |
| `reduce_segments_by_ref::<R, _>(...)` | run a reducer over segment metadata without copying segment info |
| `SidecarPolicy` | derive/check companion sidecar paths and main-file identity |
| `SidecarPolicy::inspect_with_scan_limit(...)` | inspect with an explicit fingerprint I/O ceiling |
| `SidecarPolicy::verify(main, expected)` | the verdict alone, reading only what the policy's flags consult: with `verify_main_fingerprint` off, one `metadata()` at any main-file size; `inspect` fingerprints whenever the sidecar exists, because its report publishes the identity |
| `SidecarPolicy::verify_with_scan_limit(...)` | the same, with an explicit fingerprint I/O ceiling — reached only when `verify_main_fingerprint` is set |
| `SidecarIdentity::from_main_file(...)` | fingerprint the captured extent up to the finite 256 MiB default |
| `SidecarIdentity::from_main_file_with_scan_limit(...)` | fingerprint with a caller-selected scan ceiling |
| `AdapterCheckReport` | compose physical tail status and adapter diagnostics |
| `AdapterTailStatus` | summarize expected end, available tail length, and evidence for damaged tails |
| `AdapterInputFile` | bridge path-backed and temporary byte-backed adapter inputs |
| `AdapterInputFile::from_bytes(ext, bytes)` | create an exclusive temporary input; clones keep it alive until the last drop |

The toolkit deliberately does not define TDMS object paths, scaling, DAQmx,
DataFrame/HDF export, or other domain semantics. It sits above the physical
layout layer and supplies reusable mechanics only — tagged-value codecs, chunk
index builders, segment reducers and adapter self-checks — while the adapter
author supplies the domain meaning as ordinary Rust types. TDMS is one
instantiation of those pieces, not a Varlit feature.

## Mmap And Zero-Copy

Feature-gated APIs:

| Feature | API | Meaning |
| --- | --- | --- |
| `mmap` | `unsafe mmap_payloads()` | mmap append-log payload windows |
| `mmap` | `unsafe mmap_payload_range(records)` | mmap only the records at resident positions `records` (positions relative to the range) |
| `mmap` | `unsafe VarlitFile::mmap_payload_window(entry)` | independently map one stored record payload |
| `mmap` | `unsafe mmap_matrix()` | mmap matrix slot windows |
| `mmap` | `unsafe mmap_matrix_cell::<T>(key)` | independently map one committed cell, patching only its COW pages |
| `mmap` | `MmapMatrix::cell_numeric::<T, N>(key)` | endian-aware numeric scalar read |
| `zero-copy` | `unsafe MmapPayloads::raw_fixed::<T>()` | raw fixed block view |
| `zero-copy` | `unsafe MmapMatrix::raw_cell::<T>(key)` | raw matrix cell view |

`mmap_payload_range(a..b)` maps from record `a`'s header to record `b - 1`'s
end and copies only those entries; read a large file in successive ranges, or
map `seen..record_count()?` after each `follow()`:

```rust
let mut seen = 0;
loop {
    reader.follow()?;
    let now = reader.record_count()?;
    // SAFETY: the caller guarantees no process rewrites these records while mapped.
    let fresh = unsafe { reader.mmap_payload_range(seen..now)? };
    for entry in fresh.index_entries() {
        fresh.decode_block_into(entry, &mut scratch, &mut out)?;
    }
    seen = now;
}
```

For bounded mapping workloads, choose an individual window. `MmapPayloadWindow`
owns that mapping; `as_bytes()` borrows its bytes and `offset()` reports the
payload's logical file offset. The record route validates the supplied entry
against its captured snapshot and exposes stored bytes, including compression.
The matrix route checks cell visibility and optional CRC from the held generation.
Record windows apply the selected read checks before exposing bytes, including
with `IntegrityVerification::OnDemand`. Unset policy preserves the declared CRC
checks. Whole-file mappings verify a record on first access and retain a one-byte
success flag per record when optional checks are selected; `TRUSTED` omits those
flags. Repeated checked reads reuse successful verification for
the immutable mapping's lifetime. Individual windows perform selected checks once
at construction. CRC detects corruption; it does not authenticate a sender.
`MmapLen` bounds that cell's bytes; `MaterializedBytes` bounds intersecting COW
pages, charged at 4096 bytes each. COW patches are copied before the window is
returned, so modified matrix cells do not promise zero-copy construction.

Creating any file-backed mapping is unsafe. For the mapping's entire lifetime,
the caller must prevent mutation, truncation, replacement, and backing-object
invalidation through every handle, thread, and process. The implementation
clones the already-open file handle and validates snapshot extents, but those
checks cannot enforce external immutability. Zero-copy adds unsafe marker-trait
and raw-reference contracts. Normal fixed/matrix reads use owned canonical
decoding.

Once the caller establishes that external immutability boundary, the returned
safe window APIs stay memory-safe through these enforced invariants:

1. The mapping is read-only and is created from a clone of the already-open file
   handle, so a path rename or substitution cannot redirect the mapping.
2. Record, footer, matrix, and slot extents use checked arithmetic and are
   validated against the actual mapped length before exposure.
3. `MmapPayloads` owns a copied snapshot index and rejects any
   `RecordIndexEntry` that is not exactly in that snapshot.
4. Safe accessors perform checked slicing; selected matrix CRCs are verified
   before committed slot bytes are returned.
5. The mmap owner holds the mapping for its full RAII lifetime, and returned
   slices borrow that owner, so Rust prevents them from outliving the mapping.
6. Raw zero-copy access additionally validates registration, block kind,
   version, endian, exact size, and alignment before creating a typed reference.

These invariants describe the intended safety argument under the unsafe API's
preconditions; they are not a machine-checked proof of the mmap implementation.
If a caller cannot control every writer, it must use owned reads such as
`blocks::<T>()`, `read_payload`, or matrix typed reads instead of mmap.

## Compression API

| API | Meaning |
| --- | --- |
| `compression: variable_blocks(...)` | macro global variable-block compression |
| `CompressionPolicy::VariableBlocks` | runtime global compression |
| `BlockCompressionDescriptor` | runtime per-block compression override |
| `CompressionHeaderMode::RecordExplicit` | self-describing `VCMP` envelope per record |
| `CompressionHeaderMode::FileExplicit` | file-header compression metadata |
| `CompressionHeaderMode::FormatContract` | no metadata; static spec is contract |
| `ChunkedBytes::from_zstd_chunks` | caller-managed chunked blob helper |
| `ChunkedBytes::decode_to_vec()` | decode with the finite standard logical-payload ceiling |
| `ChunkedBytes::decode_to_vec_limited(limit)` | decode with an explicit caller ceiling |
| `ChunkedBytes::decoded_len()` | declared decoded length, without decoding or content authentication |
| `ChunkedBytes::stream_range(range, buffer, max_window_bytes, visit)` | stream selected decoded bytes through caller scratch; see [range streaming](chunk-streaming.md) |
| `ChunkedBytes::decode_into(writer, limit)` | write the whole payload one verified chunk at a time; see [verified decoding](chunk-streaming.md#writing-the-whole-payload-verified-chunk-by-chunk) |

Compression requires the `compression-zstd` feature when records are actually
compressed or decompressed. `ChunkedBytes` additionally requires `integrity` for
per-chunk CRC creation/verification.

Whole-output decoding checks frame/chunk hints against complete encoded-block
output bounds before reserving output. The decoded-output limit is separate from
encoded input, native decoder workspace and caller-retained data. Range streaming
never grows its caller scratch, but zstd still owns its context/history workspace;
it is not an API for supplying all decoder storage externally. See
[range selection and retry](chunk-streaming.md).

## Update, Merge, Compact

| API | Meaning |
| --- | --- |
| `push_op::<T>(&key, &op)` | append user-defined operation |
| `delete::<T>(&key)` | append tombstone |
| `merge_keyed_files::<T, P>(spec, base, deltas, output)` | materialize ordered shards |
| `compact_keyed_file::<T, P>(spec, input, output)` | compact one file's final keyed state |
| `compact_keyed_files::<T, P>(spec, base, deltas, output)` | compact base plus deltas directly |
| `estimate_keyed_merge::<T, P>(spec, base, deltas)` | pre-flight `KeyedMergeEstimate`, decodes no values |
| `merge_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, max_distinct_keys)` | merge, refused typed at the key ceiling |
| `compact_keyed_files_with_key_limit::<T, P>(spec, base, deltas, output, max_distinct_keys)` | base+delta compact with the same ceiling |
| `compact_keyed_file_with_key_limit::<T, P>(spec, input, output, max_distinct_keys)` | single-input compact with the same ceiling |

Conflict order is shard order, then local sequence, then record ordinal. Later
delta shards win.

### Merge working storage

Merge and compact retain bounded index buffers and one operation's decoded
values. Per-key state, encoded value history and final ordering stay in bounded
RAM buffers until full, then spill independently to private unnamed files.
Explicit finite `max_index_bytes` is honored; the unbounded default uses 2 MiB
per buffer. These are per-buffer capacities, not a process-wide RAM limit;
fixed tree-page scratch and one key/record are additional minimum working space.
`max_materialized_bytes` applies to the current operation, not every value ever
seen. An explicitly requested owned `HashMap` still has a cumulative result limit.

Scratch disk usage grows with input history; disk exhaustion can fail the run.
The output path is replaced only after the complete result is written and synced.
Shard/sequence/physical ordering, tombstones, ops, and custom key equality are
preserved. `*_with_key_limit` remains an optional work/cardinality guard, counting
tombstoned keys too. This is bounded-memory execution, not a claim of measured
petabyte throughput.

`estimate_keyed_merge` still reports record counts and a hypothetical fully
resident structural size. Its byte fields and `peak_resident_structural_bytes()`
are **not current external-merge RAM or scratch-disk estimates**. They exclude
payload heap allocations and allocator overhead.

## Migration API

| API | Meaning |
| --- | --- |
| `VarlitMigration<From, To>` | user-defined semantic conversion |
| `blocks_migrated::<From, To, M>()` | read source-version blocks and convert |
| `copy_matrix_cell_bytes_from::<From, To>()` | copy compatible matrix slot payload bytes |

Normal typed reads reject block-version mismatch. Migration is explicit.

## Diagnostics And Self-Test

| API | Meaning |
| --- | --- |
| `diagnostics()` | static spec report |
| `diagnose_file(path)` | actual file report |
| `self_test(path)` | disposable roundtrip builder |
| `check_file(path)` | generated typed inspection of a quiescent file; missing coverage cannot pass |
| `check_file_with_resource_limits(path, limits)` | same inspection with finite runtime limits |
| `FileCheckReport::{checks, preconditions, passed}` | condition results, external preconditions and strict aggregate result |
| `FormatSelfTest::run_strict()` | all block samples required; warnings and skips fail |
| `FormatSelfTest::with_block(value)` | append/read sample block |
| `with_keyed_block(value)` | append/read keyed sample |
| `with_dims(dims)` | matrix runtime dimensions |
| `with_matrix_cell(key, value)` | write/commit/read matrix sample |
| `with_uncommitted_matrix_cell(key, value)` | verify uncommitted read rejection |
| `with_matrix_aux(name, offset, bytes)` | write/read aux sample |
| `cleanup(true)` | after the run, remove only files the run itself created (its native file and `.lock`), verified by object identity |

See the [application qualification template](self-check-guide.md#application-qualification-template)
for setup, status meanings and unsupported inspection routes. This is an opt-in
inspection API, separate from ordinary I/O and formal verification.

Report domains:

| Domain | Look first at |
| --- | --- |
| `FormatDefinition` | schema declaration, ids, policies |
| `CallerUsage` | API call, key, dims, commit state, block type |
| `FeatureGate` | Cargo features |
| `FileData` | actual bytes, corruption, wrong spec for file |
| `Environment` | filesystem, locks, concurrent processes |
| `LibraryInvariant` | possible Varlit bug after simple codecs are ruled out |

The self-test is non-destructive: it claims the target with exclusive create
(one exclusively created, lock-bound handle held through matrix
initialization) and refuses a pre-existing path as a `CallerUsage` failed step
instead of truncating it. `cleanup(true)` is identity-checked: the native file
is deleted only while the pathname still resolves to the object this run
created, and the `.lock` marker only after re-acquiring it through the writer
lock protocol, so a swapped-in foreign file or a foreign-owned marker
survives.

## Feature Flags

| Feature | Enables | Stability |
| --- | --- | --- |
| `integrity` | record and matrix cell/commit-map CRC checks | stable |
| `compression-zstd` | zstd record compression | stable |
| `mmap` | mmap payload/matrix views | stable, `unsafe` entry points |
| `zero-copy` | raw mmap views; implies `mmap` | stable, `unsafe` entry points |
| `scalable-fault-injection` | fault-injection counters and hooks for the scalable-path tests | **test infrastructure**: not a production feature; the counters it exposes are `#[doc(hidden)]` |

Generation-store page and checkpoint checksums remain active in every build.
All optional features are off by default. Resident, matrix, stream and
scan-control APIs require no Cargo feature, even when using
`default-features = false`. `high-cardinality-dev` was removed in 0.10.0;
delete that entry from dependency feature lists.
`scalable-fault-injection` remains test infrastructure, outside normal API
stability expectations. The storage APIs themselves follow the same pre-1.0
minor-version policy as the rest of the library.

## Common Error Interpretation

| Error | Usually means |
| --- | --- |
| `InvalidMagic` | wrong file type or corrupt header |
| `SchemaHashMismatch` | reader spec does not match writer spec; also raised for files pinned with a computed hash from a previous hash-algorithm version |
| `BlockVersionMismatch` | block version changed without migration |
| `CompressionFeatureDisabled` | enable `compression-zstd` or disable compression |
| `IntegrityFeatureDisabled` | enable `integrity` or disable CRC policy |
| `MatrixDimensionsRequired` | matrix file creation needs runtime dimensions |
| `MatrixNotCommitted` | slot bytes exist but commit bit is clear |
| `MatrixSnapshotExpired { snapshot, checkpointed }` | a matrix reader's snapshot fell further behind than the write-ahead ring keeps; a checkpoint has written newer values in place. `follow()` or reopen, then read again |
| `MatrixChecksumMismatch { offset, expected, actual }` | a cell's slot bytes do not match their stored CRC32 |
| `MatrixCellNotWritten` | `commit_matrix_cell` on a cell whose slot was never written |
| `MatrixSidecarMismatch(reason)` | matrix sidecar rejected: wrong native identity, matrix metadata offset (reported as `"matrix layout generation"`), creation nonce, or sidecar version; regenerate the sidecar |
| `InvalidMatrixLayout` | matrix metadata, ring header or a tile commit bitmap failed its check, or the geometry does not fit this spec. A damaged commit bitmap is cleared with `clear_matrix_category`; see [recovery](recovery-model.md) |
| `MatrixSizeMismatch` | encoded matrix payload does not match slot stride |
| `FormatVersionMismatch { expected, actual }` | container/layout version is not the one this build writes; the artifact is stale and regenerable. A matrix file written with the removed `VMAT` layout is instead refused with `SchemaHashMismatch` when the format declares a schema hash |
| `KeyedChainRequiresKeyedApi { block_id }` | generic `push`/`push_info` cannot maintain the keyed offset chain; use `push_keyed`/`push_keyed_info` or the generated keyed writer |
| `LimitExceeded { resource: "variable field ids" }` | decoding charged the materialization budget for distinct variable field ids above 63 and the budget ran out |
| `LimitExceeded { resource: "merge distinct keys" }` | a `*_with_key_limit` merge/compact hit the caller's `K-ever` ceiling; nothing was published |
| `BlockSchemaFingerprintMismatch` | a block impl disagrees with the format's declared identity for that block id, or two impls share an id with different `SCHEMA_FINGERPRINT` |
| `BlockKeyednessMismatch` | two block impls share an id but disagree on keyedness |
| `PublishedButParentSyncPending` | atomic replacement published, but parent-directory durability is unconfirmed; not a rollback. Also surfaced by sidecar create/bootstrap/rebuild |
| `MatrixCommittedButHookFailed { event: Box<MatrixCommitEvent>, source }` | `write_matrix_cell_durable` committed the cell and published its generation, then the caller's post-commit hook failed. Not a rollback: retry the notification with the carried `MatrixCommitEvent`, not the whole call |
| `MatrixPublicationUncertain { event: Box<MatrixCommitEvent>, source }` | `write_matrix_cell_durable` failed while publishing, before the handle's generation advanced. The writer is poisoned and the hook did not run; reopen and read the cell before retrying |
| `MatrixCommittedButDurabilityUnproven { event: Box<MatrixCommitEvent>, source }` | `write_matrix_cell_durable` published the cell's generation and a later step of `sync()` failed. Not a rollback, and the hook did **not** run. The writer is poisoned; reopen and `sync`, then issue the notification with the carried event |
| `CommittedButDurabilityUnproven { sequence, source }` | `commit_durable` appended the transaction's commit marker and the `flush`/`sync_all` after it failed. Not a rollback: the transaction is committed and `sequence` names the marker. Retry `sync`, not the transaction — re-running it appends a second marker |
| `WriterLockMarkerNotDedicated { path, reason }` | the `<target>.lock` marker path is a symlink/reparse point, a multi-link object, or not a regular file, so Varlit refused to truncate and rewrite it. Remove or un-alias the marker path |
| `PublishedButRebindFailed { sequence, source, parent_sync }` | replacement published but the writer could not rebind and was poisoned. `parent_sync: Some(_)` additionally means the parent-directory sync for the published pathname failed in the same publication, so the rename is not yet proved durable against power loss |
| `ReplacePublicationIndeterminate` | Windows `ReplaceFileW` 1176/1177 with unresolvable pathname state; temp preserved, writer poisoned, do not blindly retry |
| `WriterLockHeld` | another writer or stale lock exists |
| `WriterLockBreakRefused` | the explicit lock policy did not prove removal was allowed |
| `ScanCancelled { progress }` | an explicit scalable scan stopped cooperatively at the reported boundary |
| `BufferTooSmall { resource, required, provided }` | caller scratch is insufficient; provide a suitable buffer and retry. A nonempty chunk range requires at least one byte |
| `InvalidChunkedRange { start, end, len }` | decoded range is reversed or exceeds the declared decoded length |
| `ErrorContextAllocationFailed { context }` | detailed error storage was refused; inspect `AllocationFailureContext` for completed or uncertain publication before retrying |

## Matrix file diagnostics

`diagnose_file` on a file whose format has matrix blocks runs
`verify_matrix_metadata()`, which checks every tile header and commit bitmap.
Each finding is reported as the warning `file.matrix.recovery_finding`, and
the counts of findings and recommended actions as the info diagnostic
`file.matrix.recovery_report`. If verification itself fails, the report ends
with the error `file.matrix.verification_failed`.

A tiled matrix allocates what it writes and does not use sparse files or range
removal, so `diagnose_file` reports no storage warnings for a matrix file.
`matrix_storage_diagnostics()`, `MatrixStorageDiagnostics` and the zero-range
and sparse-zeroing accessors on `MatrixRecoveryReport` were removed in 0.13.0.
