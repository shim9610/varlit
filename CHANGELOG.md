# Changelog

All notable repository releases are documented here. Varlit (formerly Varve) follows semantic
versioning; while the crates remain below 1.0, incompatible Rust API changes
increment the minor version.

## 0.13.0-alpha.1 - unreleased

Not yet published. `0.13.0-alpha.1` is the version on the main branch while
the tiled matrix storage is still being tested: the API and the file layouts
introduced here may change before 0.13.0, and a file written by a
pre-release is not guaranteed to open with the release.

This release has breaking API and format changes; see the
[upgrade notes](docs/api-changes.md#0130). Update the Cargo dependency
requirement to `0.13.0` to use it. Schema fingerprints are unchanged.

- Breaking: typed reads decode into a value the caller owns (`out: &mut T`)
  instead of returning one.
- Breaking: matrices are stored as tiles in the primary file, overwritten in
  place through a write-ahead ring; the `VMAT` layout and the `.vmg` companion
  file are gone. A matrix spec's schema hash now includes its tile shape, so
  matrix files written by 0.12.x are refused (with `SchemaHashMismatch` when the
  format declares a schema hash); recreate them, or convert them by reading
  with 0.12.1 and writing with 0.13 (see the migration guide). A reader that falls behind the
  ring gets `MatrixSnapshotExpired` and must `follow()`.
- Breaking: remove matrix APIs that tiled storage never used:
  `MatrixMetadataResidency`, `MatrixMetadataVerification` and the `ReadLimits`
  fields, setters and accessors for matrix metadata residency, verification,
  generation-cache and directory-cache bytes;
  `FormatSpec::matrix_fatal_forensics` and `with_matrix_fatal_forensics()`;
  `compact_matrix()`; `matrix_recovery_report()` (call
  `verify_matrix_metadata()`); `matrix_storage_diagnostics()` and
  `MatrixStorageDiagnostics`; the zero-range and sparse-zeroing accessors of
  `MatrixRecoveryReport`; and the error variants `MatrixCommitQuarantined`,
  `MatrixFatalCorruption`, `MatrixChunkNotReopenable`, `MatrixChunkClosed`,
  `InvalidMatrixChunk` and `MatrixDimensionMismatch`. See the
  [migration guide](docs/migration-guide.md#upgrading-from-0121-to-0130).
- Variable records longer than 64 KiB in formats with data checksums are stored
  chunked (record flag `0x0002`) with a CRC per 64 KiB chunk. 0.12.x rejects
  such records as having an unknown flag. Stream state sidecars written by
  0.13.0 are also not readable by earlier versions.
- Breaking: remove the indexed primary. Updating a keyed record appended a new
  version and an indexed primary could never reclaim the old ones, so a file
  grew with the number of updates rather than with its data; the replacement is
  designed in [bounded keyed storage](docs/design/bounded-keyed-storage.md).
  Removed: `VarlitIndexedWriter`, `VarlitIndexedReader`, `rebuild_disk_index`,
  `rebuild_disk_index_with_progress`, `DiskIndexRebuildReport`,
  `DiskIndexPlan`, `DiskIndexedBlock`, `DiskIndexEntry`, `DiskIndexKeyTag`,
  `VarlitDiskKey`, `disk_index_sidecar_path`, the `.vki` sidecar, the
  generated `*_indexed_*` constructors and `*Indexed*` handle types,
  `DiskIndexError::PageReclaimed` and `DiskIndexOptions::max_tail_records`.
  The `key_index` block attribute is now a compile error. Stream handles, the
  `.vks` state sidecar, native keyed files and matrices are unchanged; read
  indexed data with 0.12.1 and rewrite it (see the
  [migration guide](docs/migration-guide.md)).
- Add bounded keyed storage (opt-in). A format that declares
  `keyed_storage: in_place(free_space = N%)` (or
  `FormatSpec::with_keyed_storage`) stores each keyed block once per key
  inside the file, in a B-tree of 4 KiB pages laid out the way SQLite lays
  out a table: leaves hold the records in key order (whole up to 1 KiB,
  else their first bytes and a chain of full overflow pages), interior
  pages hold the shortest separators between neighbouring keys. Pages are
  carved from blocks appended as needed and staged whole through the
  write-ahead ring; freed pages are reused once no reader or recovery can
  show a generation that used them (the writer raises the ring's mark,
  expiring older readers). Splits and merges keep pages at least half
  full; pages freed by mass deletes stay in the file for new keys.
  Each keyed block type has its own tree under a first branch held in the
  metadata (up to 64 types), so keys carry no type id. Keys are encoded
  big-endian, so keys put in increasing order fill pages in order, and a
  record stores its key fields once (in the key, not again in the value).
  `range_keyed` (and generated `range_<block>`) visits a type's records in
  key order over any range, on writers and readers, decoding each into the
  caller's value; it allocates nothing beyond the caller's buffer.
  `write_matrix_run_bytes` writes and commits a run of matrix cells in
  storage order in one call, staging a tile row at a time and each tile's
  commit bitmap once: loading 1M 44-byte cells takes 0.24 s instead of
  2.3 s cell by cell.
  Once publications take half its ring the writer checkpoints all of them:
  one read of the ring, without re-checking frames it published, and a
  range a later publication rewrites whole is not copied (matrix
  checkpoints of every generation skip it too). A reader keeps 512 tree
  pages (2 MiB, allocated at open) of its generation in memory. The keyed ring is 2 MiB and blocks grow to 1 MiB, so a
  file carries little space it does not use.
  Repeated updates no longer grow the file (the growth the security advisory
  describes). `KeyedStorage::with_change_budget` (64 MiB by default) bounds
  one round of changes; past it the writer publishes the round itself. Generated `put_<block>`, `get_<block>` and `remove_<block>`;
  `VarlitFile::put_keyed`, `get_keyed`, `remove_keyed`, `keyed_storage_stats`;
  `KeyedStorage`, `KeyedStorageStats` and
  `Error::KeyedStorageRequiresPut`. Kani proofs cover the entry search,
  separators, varints and how much of a record a cell keeps; the layouts are checked under
  Miri on a big-endian target; `keyed_storage_model` fuzzes it against a model.
- Stream `sync` and stream reader `follow` now run through the verified
  publication drivers that the indexed writer used, so the commit and follow
  protocols checked by Kani are the ones stream handles execute.
- Fix: a stream writer's immediate policy counted the format's own creation
  nonce and manifest records, so `after_records(n)` and `after_bytes(n)` fired
  one record early after `create`; counting now starts at the first caller
  write.

- Add explicit typed file inspection and strict application self-test qualification.
  Generated registries include user-defined block types; incomplete coverage is
  reported rather than accepted. See the [setup template](docs/self-check-guide.md#application-qualification-template).
- Organize native file operations into focused implementation files while
  retaining their logical module and private boundary. Consolidate current
  documentation and keep superseded audit/release evidence in Git history.
- Merge duplicate platform tests, remove empty feature placeholders, and expand
  allocation-refusal/retry checks with eight independent readers. Require local
  documentation links and the executable grouped-block quickstart in CI.
- Align the workspace, sibling dependencies and standalone consumer lockfiles
  at 0.13.0. Valid 0.12.1 native files are read without migration; matrix
  files must be recreated (see above).
- Add `ReadValidation::{FULL, CRC_ONLY, STRUCTURE_ONLY, TRUSTED}` through
  `ReadLimits::with_read_validation`. An unset override preserves existing
  behavior. Explicit choices separate data CRC from additional structure
  checks without changing stored checksums or resource ceilings. Bounds,
  finite traversal and committed-publication guards remain in every mode;
  explicit verification, recovery and index reconstruction retain strict checks.
- Add `ChunkedBytes::decoded_len` and `stream_range` for selected decoded ranges
  through caller-owned scratch. The scratch never grows; overlapping chunks
  are checked completely, and callback output is provisional until success.
- A matrix publication is no longer limited by the write-ahead ring. One that
  would take more than half the ring streams the rest of its changes into
  overflow records and names them with small ring frames; it commits with the
  same single barrier and is still all-or-nothing for readers and after a
  crash. Previously a sync that overwrote more than the ring held failed with
  `LimitExceeded`. A publication of any size commits with one small ring
  frame naming an on-disk index of its ranges; staged bytes go from the
  caller's buffer to the file through the handle's fixed buffers, and a matrix handle
  allocates nothing after open to write, publish, follow, read or
  checkpoint. Overflow records are reused once no reader can need them, also
  after the writer is reopened, and their free space takes new tiles and
  directory pages before the file grows. The matrix layout version is now 2:
  the metadata record gains the overflow list head, and matrix files written
  by an earlier 0.13 pre-release are refused with `InvalidMatrixLayout`.
- Matrix writes and follows issue fewer system calls. The entries of an open
  ring frame collect in the writer's existing 64 KiB copy buffer and reach
  the file in one write per buffer, the frame header with them when the frame
  fits (before: two writes per staged range and two per frame); the writer's
  own reads of what it staged are served from that buffer. An idle
  `follow()` reads the file length through the handle it holds instead of
  duplicating the handle. In the format-bench `cells` scenario (64 random
  1 KiB cells per durable update, four followers) this took the writer from
  about 184 to 4 writes per update and raised the update rate by about 7 %.
- Refuse data files that are not regular files. Opening a FIFO, socket,
  device or directory by path returns an `Io` error of kind `InvalidInput`
  instead of reading it, and on Unix the open no longer waits on a FIFO with
  no peer: the data file, lock marker and sidecars open with `O_NONBLOCK` and
  the type is checked on the open handle. Document the remaining I/O progress
  assumptions and where cancellation is observed.
- Refuse narrowing conversions that could silently truncate a length or count
  written to a file. Library code converts with a check or states the bound at
  the cast; `clippy::cast_possible_truncation` is denied outside tests.
- Add `ChunkedBytes::decode_into` to write a whole payload to an `io::Write`.
  It buffers one chunk, refuses a declared total above the limit before any
  output, and writes a chunk only after its length and CRC check; a failure
  leaves the earlier verified chunks in the writer.
- Add `Decoder::decode_value` and use it for library-owned codec composition.
  Reject whole-decoder replacement, retain completed charges on errors and
  exhaust callback authority on unwind. Add exact scalar transition contracts
  and compiler checks for state writes, mutable loans and callback routing.
- Verify the actual native header/footer producer-consumer pairs with full-width
  scalar fields. Exercise successfully generated files across writer resume,
  derived payloads, endians and compatible CRC/structure read policies.
- Add `TryClone`, `try_box`, `try_box_uninit`, `try_box_with` and
  `ScanCancellationToken::try_new`. Supported allocation refusals return errors
  with partial-output cleanup. Existing infallible APIs remain available.
  `ErrorContextAllocationFailed` retains allocation-free publication facts in
  `AllocationFailureContext`; callers must inspect them before retrying writes.
- Retain native follow progress across unchanged polls and incremental growth.
  A newly discovered commit replays its pending batch before publication.
  Writer changes and incompatible file state invalidate cached progress.
  Cache validated nonresident offsets within the existing directory budget,
  spilling when needed; acceleration failure falls back to checked traversal.
- Reduce ordinary native private directory rows to 13 bytes and compact eligible
  rewritten directories within the existing working allowance. These are private
  in-memory/scratch representations, not file-format changes.
- Screen zstd frame and chunk-container allocation hints against complete block
  output bounds, including legacy frames, before reserving output buffers.
- Audit vendored package identities separately from lockfiles and document mmap
  exposure to external writes and truncation.
- Allow disposable spills on `VARLIT_SPILL_DIR`. Each spill file is limited to
  `VARLIT_SPILL_MAX_BYTES`, 4 GiB when unset; a spill past the limit returns an
  `io::ErrorKind::StorageFull` error. Without `VARLIT_SPILL_DIR` spills use the
  platform temporary directory.

- Keep native frame validation in CRC-enabled indexed point reads: matching
  record/sidecar checksums do not permit unknown record flags. Share the
  positional frame parser without adding a payload checksum pre-pass.
- Preserve mutable liveness flags in positional record entries and block/key
  chain walks, so a record marked dead reports that state consistently.
- Reserve all declared internal stream tails when an embedded manifest is
  present. Indexed deletes no longer fail after writing their tombstone due
  to an undersized companion capacity. Accept the exact legacy capacity and
  upgrade it in the next ordinary write; clean opens and recovery need no
  primary scan or separate migration write.
- Preserve the format contract when skipping the redundant CRC pre-pass in
  typed streaming reads and disk-index rebuilds. `FormatContract` compression
  with CRC no longer rejects valid files with a schema-hash mismatch; consumed
  payloads still receive one checksum-verified read.
- Verify native record CRCs before returning mmap or raw fixed-block views.
  Whole-file mappings cache successful checks for their immutable lifetime;
  individual payload windows verify at construction.
- Bound matrix generation directory traversal by both checkpoint size and the
  declared scan budget, including empty aliased subtrees. Propagate malformed
  COW allocation-directory errors instead of silently omitting their evidence.
- Validate native mutation boundaries and rewrite sources, and bound disk-index
  descent using checked child metadata and finite visit budgets. Tests
  cover hostile metadata, allocation refusal, short reads, I/O failure, cleanup,
  retry and independent readers around publication.
- Add required Kani contracts and deliberate-defect controls, Loom cancellation
  models, coverage reports and dependency inventories. Audit all existing Miri
  exclusions against their native/ASan coverage and remaining gaps. See the
  [verification guide](docs/verification.md) for exact scopes and commands.
- Native handles allocate their read buffers when they open and reuse them. A
  format with variable blocks and no compression holds about 144 KiB from open
  (two 64 KiB chunk buffers and a 16 KiB read window); compression adds a
  compressed-chunk buffer and a decompression context. The read paths listed
  in `crates/varlit/tests/read_allocations.rs` make no allocator calls once the
  handle and the caller's values have been used once. Keyed passes such as
  `key_tail_offsets_into` still allocate their result.
- When a byte, string or numeric vector field must grow, its old buffer is
  freed before the new one is allocated, so the two are not live together.
- `VarlitFile::verify_all` no longer grows the handle's buffer to the largest
  record: records longer than the buffer are checksummed by streaming them.
- On a handle without a record list, `VarlitFile::verify_all` and paged
  `blocks`/`blocks_from` check and decode small records from the read window
  that framed them instead of reading each record again. In `mixed_load_bench`
  on a 5.5 GiB file, read system calls fell from 2.68 M to 36 k for a paged
  pass over 2.65 M fixed records, and from 2.77 M to 137 k for `verify_all`.
- `VarlitFile` appends no longer copy a chunked record's payload into a second
  buffer, and unlinked deletes reuse the writer's buffer for the tombstone.
  For the cases in `crates/varlit/tests/write_allocations.rs` (no
  compression), appends, deletes, flushes and syncs make no allocator calls
  after warm-up. Stream writers are unchanged.
- Fix `merge_keyed_files`, `compact_keyed_file(s)` and the merge estimate failing
  with `UnexpectedEof` on inputs large enough to open from an anchor.
- Fix `follow()` failing with `CorruptTail` when it met the writer's next record
  half written under a format without a commit marker (matrix cell-bitmap
  formats included); it now stops before that record and reads it on a later
  call. Opening a file still reports a torn tail.
- Fix `MmapMatrix::cell_payload_window` refusing cells whose slot an anchor
  splits (`MmapPayloadOutOfBounds`); such a cell is copied once on first use.
- `diagnose_file` no longer reports `matrix.storage.sparse_disabled`,
  `matrix.storage.allocation_map_unavailable` and
  `matrix.storage.hole_punch_unavailable` on every matrix file. They described
  the removed layout; tiled storage uses none of those capabilities.
- `diagnose_file` on a matrix file now runs `verify_matrix_metadata()`, which
  checks every tile header and commit bitmap, and reports each finding as
  `file.matrix.recovery_finding`. If verification itself fails it reports the
  error `file.matrix.verification_failed`. It previously reported no matrix
  findings.
- Refuse a matrix write-ahead ring header whose checksum holds but whose
  fields are impossible (offsets outside the ring, unordered or saturated
  generations) and a ring placed outside the file, with `InvalidMatrixLayout`.
  Such a file previously overflowed an offset computation at open, which
  panics in builds with overflow checks.
- Refuse a matrix tile directory root or entry that cannot be a file offset,
  and a directory height above eight, with `InvalidMatrixLayout`. Directory
  pages carry no checksum; such a value previously overflowed an offset
  computation (found by `matrix_arbitrary`).
- Verification of tiled matrix storage: ten Kani harnesses (ring headers and
  offsets, frame stepping, frame placement, frame entry bounds, slot ranges,
  Morton keys, metadata region ends), two Loom models of the tile reader lock,
  and a test that kills a writer process mid-publication and reopens the file.
- 32-bit and big-endian targets are checked in CI as unstable support: the
  workspace's tests run as `i686-unknown-linux-gnu` binaries, and Miri
  interprets the unsafe modules for i686 and s390x and the
  byte-order-sensitive unit tests for s390x. Nothing was run on such hardware.
- `varlit-core` no longer converts a file-sized `u64` to `usize` with `as`:
  263 conversions go through checked helpers, so a value that does not fit a
  32-bit address space is refused with `LengthOverflow` (or was a format
  constant, checked at compile time) instead of losing its high half. 64-bit
  behaviour is unchanged.
- The crates deny `unsafe_code`: `varlit` and `varlit-macros` forbid it, and
  `varlit-core` allows it only at declared sites, each with a reason. The
  codec's raw-memory code moved into one module with safe functions.
- Miri interprets the unit tests of the modules containing `unsafe` code it can
  execute instead of every portable unit test, and refuses to run when
  `unsafe` appears outside `scripts/miri-scope.json`. The run takes minutes
  instead of exceeding its five-and-a-half-hour limit.
- A bulk vector decode whose size computation overflows now leaves the
  destination empty, as every other refused fill does.
- Verus verifies the matrix tile slot arithmetic in the production source for
  every admitted block shape (`tools/verus`, run in CI). The specifications
  are behind a cfg only Verus sets; ordinary builds are unchanged.
- The matrix writer's frame placement and its checkpoint's room estimate now
  share one rule; they differed by one byte when the oldest kept frame started
  at ring offset 0, which could cost one extra checkpoint. Matrix metadata
  layout uses checked arithmetic and refuses a layout that would overflow.
- Refuse matrix overlay ranges whose offset would overflow with
  `InvalidMatrixLayout` instead of panicking (found by `matrix_arbitrary`).
- Add the `mixed_load_bench` example: a concurrent writer and follower on a
  file of a given size, reporting per-phase heap peak, allocator calls and read
  system calls.

Earlier releases are recorded in the [version history](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/CHANGELOG.md).
