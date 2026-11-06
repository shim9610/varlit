# API Changes

## 0.13.0

Update the Cargo dependency requirement from `0.12.1` to `0.13.0` to use these
additions. Cargo does not cross a pre-1.0 minor version automatically.
Existing public signatures, schema fingerprints, native/sidecar bytes, VCHK,
VMAT v5 and experimental
STREUSE2 formats are preserved. Rust 1.95 remains the minimum supported version.
Existing valid files need no format migration. Stricter validation can reject
damaged metadata that an older reader accepted.

| Addition | Application use |
| --- | --- |
| `ReadValidation` and `ReadLimits::with_read_validation` | Select data CRC and additional structure checks independently; leave unset to retain existing behavior |
| `ReadLimits::read_validation` and `effective_read_validation` | Inspect the optional override and resolve record-payload checks against the stored integrity policy |
| `ChunkedBytes::decoded_len` and `stream_range` | Read a selected decoded range with a fixed caller buffer after whole-output admission/refusal |
| `TryClone::{try_clone, try_clone_slice}` | Clone supported owned values with allocation refusal and partial-output cleanup |
| `try_box`, `try_box_uninit`, `try_box_with` | Return Box allocation refusal; optionally reserve before initialization |
| `ScanCancellationToken::try_new` | Create a cancellation token with a fallible shared allocation |
| `Error::BufferTooSmall`, `InvalidChunkedRange` | Distinguish empty streaming scratch from an invalid decoded range |
| `Error::ErrorContextAllocationFailed`, `AllocationFailureContext` | Preserve completed or uncertain publication facts when detailed error storage is refused |
| `AppendOnlySession`, `AppendOnlyWriter`, `AppendOnlyReaderToken`, `AppendOnlyReader` (`mmap`) | Admit native authority once; open readers before a writer and retain protected views across committed writer reconnect |
| `AppendOnlySessionStatus`, `AppendOnlyWriterState`, `AppendOnlyFailure` (`mmap`) | Observe writer lifecycle and latched failures independently of data publications |
| `GuardedMmapWindow`, `GuardedMmapPayloads` (`mmap`) | Keep native writer exclusion alive until the final protected owner drops |

`ReadLimits` and `Error` were already non-exhaustive. Existing `Clone`, token
`new`/`default` and convenience builders keep their original contracts. Internal
shared owners now use fallible allocation through a private `triomphe` dependency;
no dependency-specific owner type is exposed in place of a public std `Arc`.

### Append-only mapped access

`unsafe AppendOnlySession::new(VarlitFile)` is a lasting promise to exclude
nonparticipating mutation and invalidation. It requires a healthy, natively
locked writer, transaction markers and no matrix storage. Existing
`AppendOnlyWriter::new` remains the writer-first convenience. Derived mappings
are safe under this contract; there is no mutable inner handle or editable
conversion.

The session can issue readers before the first writer. Clones share one writer
slot. A committed healthy writer close returns cached native state; reconnect
needs no file reopen or rescan. Retain `writer.session()` when starting through
the writer-first constructor. `status`/`session_status` and `wait_for_change`
report lifetime/error snapshots, not new data. Failures stay latched across
reconnect; commit durability failures may already have published records.

Pending close, writer panic and fatal append failures require recovery.
Drop all protected readers/maps, keep the session/token, call `recover`, and
acquire a new writer. `AppendOnlyWriterActive`, `AppendOnlyReadersActive` and
`AppendOnlySessionBusy` distinguish conflicting activity;
`AppendOnlyRecoveryRequired` requests reconstruction, `AppendOnlySessionClosed`
ends writer issuance, and `AppendOnlySessionPoisoned` fails closed on coordination
poisoning. Recovery never releases the pinned native lock; it
excludes new reader registration and refuses existing read protection.
`close` on the session permanently revokes writer issuance. Read tokens/maps
retain native exclusion after write authority and its buffers end.

`open_reader_with_resource_limits` overlays supplied operational options on
format defaults: use `ReadLimits::MISSING` to preserve other fields. Bounds,
publication and ownership remain required in every read mode. Existing
unguarded mapping constructors retain their `unsafe` contracts. See
[the lifecycle and error table](api-reference.md#append-only-mmap-sessions) and
[runnable example](../crates/varlit/examples/append_only_mmap.rs).

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
`VARLIT_SPILL_MAX_BYTES` optionally limits each spill file's logical length;
unset remains uncapped. Database size is not capped, and atomic replacement
files still live beside their destination. Configure these variables before
starting handles or worker threads. See [spill policy](scalable-io.md#private-spill-policy).

Native follow retains checked scan progress, and native marks reuse validated
boundary metadata where available. Initial admission and invalidated caches can
still require a scan. Ordinary directory rows are now 13 bytes; eligible rewritten
directories can use a compressed private representation. These changes do not
alter persistent user-file formats or increase configured budgets.

Earlier release notes are available in the [version history](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/api-changes.md).
