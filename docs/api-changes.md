# API Changes

## 0.13.0

Update the Cargo dependency requirement from `0.12.1` to `0.13.0`. Cargo does
not cross a pre-1.0 minor version automatically. Rust 1.95 remains the minimum
supported version. This release has breaking changes:

- Typed reads decode into a value the caller owns (see
  [caller-owned decoding](#caller-owned-decoding)).
- Matrix storage is tiled and the `VMAT` layout is removed. A matrix spec's
  schema hash now includes its tile shape, so matrix files written by 0.12.x
  are refused (with `SchemaHashMismatch` when the format declares a schema
  hash); recreate them.
- Stream sidecars written by 0.13.0 are not readable by earlier versions.
- The indexed primary is removed (see
  [indexed primary removal](#indexed-primary-removal)). Indexed files are not
  converted: read their data with the release that wrote them and rewrite it
  as a native file, a stream or a matrix.

VCHK and experimental STREUSE2 formats are unchanged. In a format with data
checksums, a variable record longer than 64 KiB is now written with a CRC per
64 KiB chunk (a `VRCK` table inside the payload, marked by a new record flag);
0.12.x refuses that flag as unknown. Stricter validation can reject damaged metadata that an
older reader accepted.

| Addition | Application use |
| --- | --- |
| Generated `check_file`, `check_file_with_resource_limits`; `FileCheckReport` | Inspect API-produced files with automatic typed registry coverage and explicit violated/unverified conditions |
| `FormatSelfTest::run_strict` | Require every declared block sample, strict read checks, no warnings or skipped steps; existing `run` is preserved |
| `ReadValidation` and `ReadLimits::with_read_validation` | Select data CRC and additional structure checks independently; leave unset to retain existing behavior |
| `ReadLimits::read_validation` and `effective_read_validation` | Inspect the optional override and resolve record-payload checks against the stored integrity policy |
| `ChunkedBytes::decoded_len` and `stream_range` | Read a selected decoded range with a fixed caller buffer after whole-output admission/refusal |
| `ChunkedBytes::decode_into` | Write the whole payload to an `io::Write`, buffering one chunk and writing only chunks that passed their length and CRC checks |
| `Decoder::decode_value` | Compose custom codecs with checked decoder identity and retained input/budget authority |
| `TryClone::{try_clone, try_clone_slice}` | Clone supported owned values with allocation refusal and partial-output cleanup |
| `try_box`, `try_box_uninit`, `try_box_with` | Return Box allocation refusal; optionally reserve before initialization |
| `ScanCancellationToken::try_new` | Create a cancellation token with a fallible shared allocation |
| `Error::BufferTooSmall`, `InvalidChunkedRange` | Distinguish empty streaming scratch from an invalid decoded range |
| `Error::ErrorContextAllocationFailed`, `AllocationFailureContext` | Preserve completed or uncertain publication facts when detailed error storage is refused |

`ReadLimits` and `Error` were already non-exhaustive. Existing `Clone`, token
`new`/`default` and convenience builders keep their original contracts. Internal
shared owners now use fallible allocation through a private `triomphe` dependency;
no dependency-specific owner type is exposed in place of a public std `Arc`.


### Caller-owned decoding


**Breaking.** A read
that returned a decoded block now takes `out: &mut T` and decodes into it,
reusing the vectors and strings `out` already owns. A steady read loop keeps one
`out` and one `scratch` alive and allocates nothing per record once both have
grown to the largest record they hold. `scratch` holds unchunked records and the
small fields of chunked ones; large byte and numeric fields of a chunked record
are read straight into `out`. File bytes, schema fingerprints and
materialization limits are unchanged; an in-place decode is charged exactly what
a fresh decode is charged and is refused at the same limit.

| Before | Now |
| --- | --- |
| `BlockVec::get(i) -> Result<Option<T>>` | `get(i, &mut out) -> Result<bool>` |
| `BlockVec::get_into(i, &mut payload) -> Result<Option<T>>` | `get_into(i, &mut payload, &mut out) -> Result<bool>` |
| `BlockVec::iter()` / `BlockIter` (an `Iterator`) | `cursor()` / `BlockCursor::next_into(&mut out) -> Result<bool>` |
| `KeyedBlockVec::get(&k) -> Result<Option<T>>` | `get(&k, &mut out) -> Result<bool>` |
| `StreamingBlocks<T>`: `Iterator<Item = Result<T>>` | `next_into(&mut out) -> Result<bool>`; no longer an `Iterator` |
| `read_block_at::<T>(offset) -> Result<T>` | `read_block_at::<T>(offset, &mut scratch, &mut out) -> Result<()>` |
| `decode_block_into::<T>(&entry, &mut scratch) -> Result<T>` | `decode_block_into::<T>(&entry, &mut scratch, &mut out) -> Result<()>` |
| `read_header_block::<T>() -> Result<Option<T>>` | `read_header_block::<T>(&mut out) -> Result<bool>` |
| `read_matrix_cell::<T>(key) -> Result<T>`, generated cell getters | `read_matrix_cell::<T>(key, &mut out) -> Result<()>` |
| experimental `MatrixReader::get::<T>(key) -> Result<T>` | `get::<T>(key, &mut out) -> Result<()>` |
| `decode_blocks_into(&mut Vec<T>)` (cleared, then pushed) | same signature; existing elements are decoded into, the vector only grows past its old length |

`false` means "absent" (no record at that index, key or header slot, or end of
walk) and leaves `out` untouched. After an error `out` is a valid value with
unspecified contents.

### Durability and reads

| Change | Effect |
| --- | --- |
| `transaction_marker(<mode>, single_barrier)` (`TransactionMarkerMode::{OnFlushSingleBarrier, ExplicitSingleBarrier}`, `CommitPolicy::single_barrier`) | `commit_durable` with one storage barrier instead of two; open verifies the newest transaction before committing it. Opt-in; requires `crc32_with_header`. See [durability model](durability-model.md#single-barrier-commit) |
| Stream `sync()` | One storage barrier on the primary per sync, closed by a sync marker. The sidecar is written and synced only when the primary has grown past `DiskIndexConfirmPolicy` (default 64 MiB or 65,536 records) and at compaction; readers and recovery replay the rest from the primary. A clean sidecar with a longer native file now means "restore", not `NativeLengthMismatch`. Sidecars written by this version are not readable by earlier versions |
| `MmapPayloads::decode_block_into(&entry, &mut scratch, &mut out)` | Decode straight from a read-only mapping, with the same id/version, checksum, length and materialization checks as the copying read |
| Tiled matrix storage | Cells live in tiles of the spec's `tile = [rows, cols]` shape and are overwritten in place through a write-ahead ring inside the file; overwrites do not grow the file (`overwrites_keep_the_file_size_and_new_tiles_add_one_tile`). A reader that falls behind the ring gets `MatrixSnapshotExpired`. The `VMAT` layout and its generation log are removed; 0.12.x matrix files are refused (with `SchemaHashMismatch` when the format declares a schema hash). `FormatSpec::with_matrix_ring_bytes` sets the ring size. See [durability model](durability-model.md#matrix-storage-and-publication) |
| Matrix settings and errors left from `VMAT` | Removed: `MatrixMetadataResidency`, `MatrixMetadataVerification` and the `ReadLimits` matrix residency, verification, generation-cache and directory-cache fields, setters and accessors; `FormatSpec::matrix_fatal_forensics` and `with_matrix_fatal_forensics()`; `compact_matrix()`; `matrix_recovery_report()`; `matrix_storage_diagnostics()` and `MatrixStorageDiagnostics`; the zero-range and sparse-zeroing accessors of `MatrixRecoveryReport`; and the error variants `MatrixCommitQuarantined`, `MatrixFatalCorruption`, `MatrixChunkNotReopenable`, `MatrixChunkClosed`, `InvalidMatrixChunk` and `MatrixDimensionMismatch`. Open runs no matrix verification; `verify_matrix_metadata()` checks tiles on request and returns a `MatrixRecoveryReport`. `matrix_generation_path()` returns the primary path. See [migration guide](migration-guide.md#upgrading-from-0121-to-0130) and [recovery model](recovery-model.md) |
| `diagnose_file` on a matrix file | Runs `verify_matrix_metadata()` and reports each finding as `file.matrix.recovery_finding`; a failed verification is the error `file.matrix.verification_failed`. It previously reported no matrix findings |
| `unsafe mmap_payload_range(records)`, `record_count()` (`VarlitFile`, `VarlitReader`) | Map only the records at resident positions `records`. `MmapLen`, `Records` and `IndexBytes` are charged for the range, not the file; a following reader maps only `seen..record_count()?`. Out-of-range positions return `MmapRecordRangeOutOfBounds` |

Additions supporting this:

| Addition | Use |
| --- | --- |
| `VarlitEmpty` (`T::empty()`) | A starting value for `out`; derived for every `VarlitBlock` and finite key, implemented for scalars, `String`, `Vec`, `Option`, arrays, maps and tuples up to 4. A hand-written `VarlitBlock` used as an output implements it itself |
| `VarlitDecode::decode_varlit_into` | In-place decode hook; the default decodes a fresh value and assigns it. Derived blocks, `Vec` of fixed-width values, `Vec<u8>`, `String`, `Option`, `Vec<String>` and record sequences override it to reuse capacity |
| `Decoder::decode_value_into`, `decode_nested_into`, `decode_from_slice_limited_into`, `decode_from_slice_accounted_into`, `decode_from_slice_into` | In-place counterparts of the value decodes, through the same checked decoder frame and budget |
| `Vec<T: VarlitBlock>` as a block field | Whole records as one field (compound-array layout), bounded and fallible like other sequences |

### Lazy open and explicit scan selection

`VarlitFile::open_readonly_lazy`, `VarlitFile::open_lazy`, their report variants,
and the `VarlitWriter` lazy facade now return errors without an automatic full
scan. This is a behavior change with unchanged signatures and file bytes.
The selected summary is header tails when declared, otherwise the trailing digest.

Callers can match `LazyOpenMetadataUnavailable { metadata }` or
`LazyOpenVerificationRequiresScan` and choose `open_readonly_scan` / `open_scan`.
Other errors retain their variants. Directory-free `reopen_readonly` uses the
same strict lazy policy. Explicit scans also return errors on failure;
a writer scan can perform normal uncommitted-tail recovery.

To retain the previous automatic behavior, use `open_readonly_lazy_or_scan` or
`open_lazy_or_scan`, optionally suffixed `_with_report`. These opt-in APIs can
report `LazyOpenSource::FullScan`. Normal native `open`/`open_readonly` scan APIs
and stream constructors retain their existing selection and return types.
See the [API contract](api-reference.md#an-open-that-reads-no-record).

### Indexed primary removal

**Breaking.** The indexed primary — a native file plus a `.vki` sidecar holding
a disk B-tree of the latest record per key — is removed. Updating keyed records
grew its files without bound, and it could never reclaim superseded versions.
The replacement design is [bounded keyed storage](design/bounded-keyed-storage.md);
it is not implemented.

| Removed | Use instead |
| --- | --- |
| `VarlitIndexedWriter`, `VarlitIndexedReader`; generated `create_indexed_writer`, `open_indexed_writer`, `restore_indexed_writer`, `open_indexed_reader` and the `<Format>IndexedWriter` / `<Format>IndexedReader` types | Stream handles (`create_stream_writer`, `open_stream_writer`, `restore_stream_writer`, `open_stream_reader`) for append and follow; resident handles (`create_writer`, `open_reader`, `keyed_blocks`) for lookup by key |
| `rebuild_disk_index`, `rebuild_disk_index_with_progress`, `DiskIndexRebuildReport` and the generated `rebuild_disk_index` | `bootstrap_stream_checkpoint` for a stream file's `.vks` |
| Lookup by key on a sidecar: generated `get_<block>`, `lookup_entry`, `historical_distinct_keys`, indexed `compact_index` | Resident `keyed_blocks::<T>()?.get(&key, &mut out)` or the generated `<blocks>()?.get(&key, &mut out)` |
| `DiskIndexPlan`, `DiskIndexedBlock`, `DiskIndexEntry`, `DiskIndexKeyTag`, `VarlitDiskKey` | None |
| `DiskIndexError::PageReclaimed`, `DiskIndexOptions::max_tail_records` | None |
| Block attribute `key_index = disk` / `key_index = memory` | Delete it; it is now a compile error. Keyed blocks without it keep working |
| `.vki` sidecar files | Delete them after rewriting the data |

Stream handles, `StreamOptions`, `DiskIndexOptions`, `DiskIndexBatchOptions`,
`DiskIndexConfirmPolicy`, `DiskIndexError` (without `PageReclaimed`),
`DiskIndexMode`, `IndexCompaction`, `SnapshotStatus` and
`bootstrap_stream_checkpoint` are unchanged. Stream `sync()` and stream reader
`follow()` now run through the Kani-verified `commit` and `follow` drivers in
`crates/varlit-core/src/publication_protocol.rs`. A stream writer's Immediate
policy counters start at the first caller write; the creation nonce and
embedded manifest records no longer count. Native and matrix files are
unchanged. See the [migration guide](migration-guide.md#indexed-primary-removed).

### Composing custom decoders

Use `decoder.decode_value::<T>()?` to compose codecs. Library-owned containers and
generated codecs use this boundary, including custom `decode_array` overrides.
Completed charges remain spent on ordinary errors. Replacing the borrowed decoder
with a fresh value is rejected even when the public settings match. Invalid
replacement or a panicking callback exhausts that frame's cursor and budget;
catching the panic cannot refund them. Nested decoding preserves the parent's
input. Accounted decoding exhausts the shared ledger before entering a callback
and restores only the admitted remaining amount on return.

Existing trait implementations keep compiling. The hidden defaulted
`VarlitDecode::__VARLIT_BUILTIN` constant and opaque `BuiltinCodec<T>` evidence are
reserved for library-owned implementations; safe downstream code cannot construct
or transfer evidence to a different type. This removes redundant checks around
built-in scalar composition without skipping input/resource validation. Directly
calling `T::decode_varlit` remains possible and places composition responsibility
on that caller. User callback allocation, termination and destructor behavior are
outside this boundary's guarantee. See the [state contract](verification/state-transitions.md).

### Selecting read checks

Use `ReadLimits::MISSING.with_read_validation(policy)` with a runtime resource
override to change only the check selection. Passing `STANDARD` also supplies
standard resource values, which can replace a format's declared values. Ordinary
opens preserve the unset compatibility behavior; they do not implicitly select
`TRUSTED`. `FULL` and `CRC_ONLY` require stored CRCs and the `integrity` feature.

The option changes read-side consistency work, not wire identity, writer CRCs,
allocation ceilings or publication boundaries. `TRUSTED` does not authorize
`TrustedUnbounded`. Explicit verification, recovery and index reconstruction do
not inherit an unchecked payload-read choice. See the
[API selection table](api-reference.md#read-validation) and
[I/O contract](design/read-validation-policy-review.md).

### Retrying after an error

For whole-output chunk refusal, the caller can choose a range and a smaller
buffer using [range streaming](chunk-streaming.md). This requires an already
available `ChunkedBytes`; it does not bypass the enclosing record's limits.
Callbacks may receive bytes before a later checksum error, so discard staged
output on failure and restart the requested range on retry.

For writes, inspect the operation result before retrying. In particular,
`ErrorContextAllocationFailed` can describe an append that completed, a durable
generation, or uncertain publication. It does not mean that no mutation occurred.
Existing writer-poisoning and recovery rules still apply; see
[error interpretation](api-reference.md#common-error-interpretation).

### Temporary storage and repeated reads

`VARLIT_SPILL_DIR` selects an existing writable directory for disposable spills.
`VARLIT_SPILL_MAX_BYTES` limits each spill file's logical length; unset, the
limit is 4 GiB. Database size is not capped, and atomic replacement
files still live beside their destination. Configure these variables before
starting handles or worker threads. See [spill policy](scalable-io.md#private-spill-policy).

Native follow retains checked scan progress, and native marks reuse validated
boundary metadata where available. Initial admission and invalidated caches can
still require a scan. Ordinary directory rows are now 13 bytes; eligible rewritten
directories can use a compressed private representation. These changes do not
alter persistent user-file formats or increase configured budgets.

Earlier release notes are available in the [version history](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/api-changes.md).
