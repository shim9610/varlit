# Changelog

All notable repository releases are documented here. Varlit (formerly Varve) follows semantic
versioning; while the crates remain below 1.0, incompatible Rust API changes
increment the minor version.

## 0.13.0 - 2026-11-05

This release has breaking API and format changes; see the
[upgrade notes](docs/api-changes.md#0130). Update the Cargo dependency
requirement to `0.13.0` to use it. Schema fingerprints are unchanged.

- Breaking: typed reads decode into a value the caller owns (`out: &mut T`)
  instead of returning one.
- Breaking: matrices are stored as tiles in the primary file, overwritten in
  place through a write-ahead ring; the `VMAT` layout and the `.vmg` companion
  file are gone. A matrix spec's schema hash now includes its tile shape, so
  matrix files written by 0.12.x are refused (with `SchemaHashMismatch` when the
  format declares a schema hash); recreate them. A reader that falls behind the
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
  such records as having an unknown flag. Stream/indexed sidecars written by
  0.13.0 are also not readable by earlier versions.

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
  after warm-up. Stream and indexed writers are unchanged.
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
- Verification of tiled matrix storage: ten Kani harnesses (ring headers and
  offsets, frame stepping, frame placement, frame entry bounds, slot ranges,
  Morton keys, metadata region ends), two Loom models of the tile reader lock,
  and a test that kills a writer process mid-publication and reopens the file.
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
  system calls. The storage benchmark harness now follows and retries indexed
  lookups that meet `DiskIndexError::PageReclaimed`, as that error documents.

Earlier releases are recorded in the [version history](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/CHANGELOG.md).
