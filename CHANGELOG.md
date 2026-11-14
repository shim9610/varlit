# Changelog

All notable repository releases are documented here. Varlit (formerly Varve) follows semantic
versioning; while the crates remain below 1.0, incompatible Rust API changes
increment the minor version.

## 0.13.0 - 2026-11-05

Existing public method signatures, schema fingerprints and file formats are
preserved. Update the Cargo dependency requirement to `0.13.0` to use this release.
See the [upgrade notes](docs/api-changes.md#0130) for API selection and
error-handling changes.

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
  at 0.13.0. This release requires no migration of valid 0.12.1 files.
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
- Allow disposable spills on `VARLIT_SPILL_DIR` with an optional per-file
  `VARLIT_SPILL_MAX_BYTES` policy. Defaults retain platform temporary storage
  without an additional capacity cap; database size and file format are unchanged.

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
  descent using checked child metadata and finite visit budgets. Compound tests
  cover hostile metadata, allocation refusal, short reads, I/O failure, cleanup,
  retry and independent readers around publication.
- Add required Kani contracts and deliberate-defect controls, Loom cancellation
  models, coverage reports and dependency inventories. Audit all existing Miri
  exclusions against their native/ASan coverage and remaining gaps. See the
  [verification guide](docs/verification.md) for exact scopes and commands.

Earlier releases are recorded in the [version history](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/CHANGELOG.md).
