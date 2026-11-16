# Scalable Stream I/O

This guide covers the stream handles of the default public API. They are the
Varlit path intended for very large append logs. Their normal open, append and
follow paths do not rescan the complete native prefix. A stream file has a
`<file>.vks` state sidecar that holds bounded resume state: the confirmed EOF,
record count, next sequence and one tail per declared block. It holds no keys.
Resident buffers are bounded; sidecar size, publication frequency and the
number of declared blocks still affect publication work.

> **Keyed lookup.** Stream readers have no lookup by key. The indexed primary
> (`VarlitIndexedWriter`, `VarlitIndexedReader`, the `.vki` sidecar and the
> `key_index` block attribute) was removed: updating keyed records grew its
> files without bound and it could never reclaim superseded versions. Keyed
> lookup is now resident only. The replacement design is
> [bounded keyed storage](design/bounded-keyed-storage.md); it is not
> implemented. See the [migration guide](migration-guide.md) for existing
> indexed files.

The resident `create_writer`, `open_reader`, `blocks`, and `keyed_blocks`
APIs can still scan the complete file at open. Their ordinary 13-byte private rows use
bounded private buffers (2 MiB by default, or the declared finite index budget)
and spill excess rows to temporary storage. Bounded RAM does not make that scan
independent of file size.

Opt-in policies narrow that gap without closing it. `segment_on_flush` makes
open frame one record per commit point instead of one per record;
`open_digest_on_flush` plus `VarlitFile::open_readonly_lazy` makes open frame one
record and retain **nothing**, leaving the caller to build only the part of the
index its question needs with `record_map`. Lazy open returns errors without
automatic scanning; explicitly choose `open_readonly_scan` / `open_scan`, or
`*_lazy_or_scan[_with_report]` for the former automatic recovery behavior.
These policies change which metadata
is read at open; this guide does not publish measurements for them.

Ordinary keyed merge/compact also spills bounded index buffers, but still needs
scratch disk and per-record decoding workspace. A `record_map` walk remains a
forward walk of the record chain — `O(records before the answer)`, rather than a
keyed lookup. Stream handles keep primary open/append work independent of the
complete native directory; choose a digest when native open scans are the
immediate cost to reduce.

> **API availability.** Stream handles, finite keys and scan control are part
> of the default public API, also with `--no-default-features`. Remove
> `high-cardinality-dev` from Cargo manifests.
> Repository release and long-duration load qualification are tracked separately.
> A release tag does not establish a completed endurance test.
> Explicit `compact_index()` reclaims obsolete sidecar pages; whole-native-log
> keyed merge/compact uses bounded buffers and private spill files.
> See [Known Limitations §3](known-limitations.md#3-stream-api-status)
> and [§6](known-limitations.md#6-not-verified) for the remaining scope.

This guide describes Varlit 0.13.0, including read-check selection and fallible
token construction. See [API changes](api-changes.md#0130).

## Private spill policy

Set `VARLIT_SPILL_DIR` to an existing writable directory to choose the volume
for disposable directory, merge and index scratch files. Otherwise the platform
temporary directory is used. Invalid explicit paths return an error without
falling back to another location. Set process configuration before opening
handles or starting worker threads; each new spill captures the current policy.

`VARLIT_SPILL_MAX_BYTES` is an optional unsigned decimal ceiling on the logical
length of **each** private spill file. Unset applies a 4 GiB default ceiling;
`0` refuses nonempty spills. It is not a database-size limit, total directory quota, free-space
reservation or process-memory limit. Filesystem-full and partial-write errors
still propagate. Use a filesystem quota if an aggregate limit is required.
Files used for atomic replacement stay beside their destination. On `tmpfs`,
temporary storage consumes memory outside ordinary process resident set size
(RSS) accounting.

The [self-check guide](self-check-guide.md#scalable-io-validation) gives commands
for model histories, concurrent snapshots, external-kill recovery, memory budgets
and actual-file write/read/delete validation. Passing a bounded campaign does
not establish power-loss safety or long-duration endurance.

## Declare The Format

```rust
use varlit::varlit_format;

varlit_format! {
    pub format Capture {
        magic: b"CAPT";
        version: 1;
        schema_hash: computed;

        blocks {
            fixed Run(id = 1) {
                id: u64,
            }

            variable Frame(id = 2) {
                scan: u32,
                frame: u32,
                payload: Vec<u8>,
            }
        }
    }
}
```

Every format generates the stream handles; no block attribute selects them.
The block attribute `key_index` (with any value) is a compile error: keyed
records no longer keep a disk index sidecar.

A stream writer generates `push_<block>` and `push_<blocks>` for every
non-matrix block. A keyed block in a format without `keyed_offset_chain` also
gets `delete_<block>(&key)`. In a `keyed_offset_chain` format the stream writer
generates no method for a keyed block, because it keeps no previous-key chain;
write those blocks through the resident writer.

Generated non-keyed block methods continue to work in a
`keyed_offset_chain` format. Generated `VarlitBlock` implementations carry an
`IS_KEYED` type fact so the runtime rejects chain-unsafe low-level batch calls
without rescanning the file.

Manual `VarlitBlock` implementations
must declare `const IS_KEYED: bool`; there is no chain-unsafe default. Set it
to `true` for every manual `VarlitKeyedBlock` implementation and `false`
otherwise. Omitting it is a compile error.

### Finite schema keys

A finite key identifies a **declared value combination**, not a record and not
merely its block type. Declare either named combinations or a Cartesian domain
on a keyed block:

```rust
fixed Frame(id = 2, key = [scan, frame],
    key_values = [Scan1Frame2 = (1, 2), Scan2Frame4 = (2, 4)]) {
    scan: u32, frame: u32, payload: u64,
}
// Alternative: 4 * 16 = 64 permitted combinations.
fixed Sample(id = 3, key = [scan, frame],
    key_domain = [scan = 0..4, frame = 0..16]) {
    scan: u32, frame: u32, payload: u64,
}
```

The enclosing format must use `schema_hash: computed;`. The macro generates
`FrameKey::{Scan1Frame2, Scan2Frame4}` and a block with **`key: FrameKey`** and
`payload: u64`. The declared `scan` and `frame` fields become that one key field;
their raw values are not repeated in the record. The generated
key occupies the first removed key field's position and field ID; other field
IDs are preserved. A non-key field named `key` is therefore rejected.

```rust
let key = FrameKey::from_values((1, 2))?;
let record = Frame { key, payload: 17 };
writer.push_frame(&record)?; // resident writer
writer.flush()?;
let mut latest = Frame::empty();
let found = reader.frames()?.get(&FrameKey::Scan1Frame2, &mut latest)?;
assert_eq!(key.values(), (1, 2));
assert!(FrameKey::from_values((1, 4)).is_err());
```

Both forms generate `COUNT`, `ALL`, `code()`, `from_code()`, `from_values()` and
`values()`. Direct lists produce the supplied variant names. Cartesian domains
produce `K0`, `K1`, etc.; use `from_values()` to avoid depending on those names.
The last field varies fastest, and all fields must be listed in key declaration
order. Lists retain their declared order; ranges support `a..b` and `a..=b`.

The hard limit is **4,096 combinations per block**, checked before multiplying
out a Cartesian product. Every code has a fixed **u16 / 2-byte** representation,
even for a small enum. This bounds generated-code/compiler cost without
introducing width transitions. It is a policy limit, not the u16 numeric limit.
A 4,096-variant integration test compiles the actual enum and checks every
combination. Compile-time and memory comparisons require measurements on the
application's toolchain and host.

Supported domain fields are `u8/u16/u32/u64`, `i8/i16/i32/i64`, `bool`, and
`String` (literal labels; conversion takes `&str`). Type aliases, expressions,
128-bit integers, and runtime-sized domains are not accepted by this syntax.
Empty domains, duplicates, wrong tuple arity, out-of-range literals, defaulted
key fields, and oversized products are compile errors. Invalid input combinations
and unknown on-disk codes return errors; they never extend a dictionary or create
an enum variant. Public block construction takes a valid enum, so there is no
unchecked raw tuple hiding in a constructed block.

Codes follow declaration order. Reordering or changing the value/code mapping
changes the key's codec schema identity, the block fingerprint, and the computed
format hash. Opening a file with a reassigned mapping is rejected. Schema
changes require an explicit migration/new file; mappings are not silently
reconciled at runtime. Renaming a variant alone does not change its on-disk
meaning.

There is no runtime value dictionary, registration API, or shared cache for
finite keys. A block's distinct keys cannot exceed its declared domain. Each
write of a key appends a new record; the finite domain bounds the number of
distinct keys, not the number of records or the file size.

The raw `key = [fields]` form without `key_values` or `key_domain` remains an
**unrestricted key** declaration. Both kinds can coexist in one format.
The mistaken `<Format>KeyKind` enum has been removed.

## Stream Write And Read

```rust
use varlit::{BatchOptions, StreamOptions};

let mut writer = Capture::create_stream_writer(
    "capture.varlit",
    StreamOptions::default(),
)?;

writer.push_run(&Run { id: 7 })?;

let report = writer.push_frames(
    (0..1_000_000u32).map(|sequence| Frame {
        scan: sequence % 4,
        frame: (sequence / 4) % 16,
        payload: Vec::new(),
    }),
    BatchOptions::default(),
).map_err(|error| error.source)?;

assert_eq!(report.records, 1_000_000);
writer.sync()?;

let reader = Capture::open_stream_reader(
    "capture.varlit",
    StreamOptions::default(),
)?;
let all_runs = reader.runs()?;       // explicit lazy native scan
let all_events = reader.events()?;  // explicit lazy native scan
let verified = reader.verify_all()?; // explicit eager full validation
# Ok::<(), varlit::Error>(())
```

Opening the reader does not *scan* the native record region, but it does read
a bounded part of it: the primary-generation witness digests the leading window
of the file — at most 4 KiB from offset 0 — and takes one point read of the
leading record for the creation nonce. It then walks the records synced after
the sidecar's confirmed EOF, which `DiskIndexConfirmPolicy` bounds (see
[Durability And Recovery](#durability-and-recovery)). Calling `runs`, `frames`,
`events`, or `verify_all` explicitly creates a native scanner; their cost is
linear in the selected snapshot.

## Batch Cost And Memory

### Choose the physical block before choosing the batch size

An application item, an encoded Varlit record, a buffered append batch, and a
durable publication are different units. `push_frames(items, BatchOptions)`
coalesces writes, but still encodes one Frame per input item. `sync()` publishes
the accumulated work; calling it less often does not remove the individual
framing.

For small samples/items normally consumed together, declare a block containing
hundreds or thousands of items and locate an item inside its payload. For
example, 1,048,576 128-byte items can be stored as 2,048 blocks of
512 items (64 KiB payload each), or 128 blocks of 8,192 items (1 MiB each).
These are still 1,048,576 logical items, not that many physical records or
durability boundaries. The block size controls framing amortization and the
bytes fetched on a cache miss. The publication size independently controls
durability cost, reader visibility latency and producer buffering.
Packing is an explicit application schema choice, not something `BatchOptions`
performs implicitly.

Use this layout when the application can identify an item's block: a block
key, fixed-size dense IDs, a record offset, or a persisted routing directory.
For unrelated arbitrary keys, grouping alone does not provide a lookup index.
Store and measure the routing/secondary index too; do not silently replace
arbitrary-key lookup with a known block offset or an uncharged RAM map.
Finite `key_domain`/`key_values` describes a bounded set of key values; it is
not a switch that packs an unbounded number of individually framed records.

A minimal declaration for application-owned packed bytes, looked up by group
through the resident keyed API, is:

```rust
use varlit::varlit_format;
varlit_format! {
    pub format PackedCapture {
        magic: b"PACK";
        version: 1;
        index: keyed_offset_chain;
        blocks {
            variable ItemBlock(id = 1, key = [group]) {
                group: u64,
                data: Vec<u8>,
            }
        }
    }
}
```

Define and version the item encoding inside `data`, including item width/count,
byte order and key-to-slot mapping. A library block checksum protects those
bytes; the application must still validate its internal layout and slot bounds.
The benchmark's executable example is
[`tools/storage-bench/examples/grouped.rs`](../tools/storage-bench/examples/grouped.rs).

For immutable published groups, a reader can fetch/verify one block and satisfy
a batch of item queries from that bounded buffer. Charge block lookup, decoding,
cache misses, item extraction and copies. This is a locality-dependent workload;
it is not equivalent to a million unrelated cold random block reads. Reusing a
just-scanned payload without a separate point fetch must also be disclosed.
Choose grouping and publication sizes against the application's latency target,
not throughput alone. Incomplete tail groups need an explicit count or a separate
publication policy; do not expose padding as valid items.

Keep one writer owner and use a bounded MPSC queue for multiple producers.
Open an independent reader handle per worker and use `follow()` plus persistent
cursors to consume new published data. Reader block buffers and caches add
across readers; include producer staging, queue contents and the active writer
when sizing memory. For native append-only files, `mmap_payload_window` and
`mmap_payloads` can capture confirmed immutable records while the writer appends
strictly after them. These unsafe APIs require the application to exclude
truncation, replacement, compaction and every operation that could modify or
invalidate captured records. Existing mappings never grow; follow publications
and map new windows or take a new snapshot. Whole-prefix snapshots copy the
captured index when rebuilt; `mmap_payload_range(seen..record_count()?)` maps and
copies only what was appended since the previous mapping, and record windows
avoid the index copy entirely.
Growing matrix layouts require their own stricter mapping contract. See
[mmap safety](../SECURITY.md#mapped-files-and-external-modification).

`BatchOptions` bounds transient native buffering:

```rust
let batch = BatchOptions {
    max_records: 16_384,
    max_bytes: 4 * 1024 * 1024,
};
```

A chunk is encoded in memory and written once with `write_all`. `write_calls`
in `BatchAppendInfo` counts those calls, not guaranteed kernel syscalls. A
single valid record larger than `max_bytes` is written alone; per-record stored
and logical payload limits still apply before allocation or I/O.

`StreamOptions::state` is a `DiskIndexOptions`. Its `batch` field independently
bounds one sidecar update batch; native chunk boundaries do not force a sidecar
commit. `confirm` (a `DiskIndexConfirmPolicy`, default 64 MiB or 65,536
records) decides how far the confirmed sidecar may trail the synced primary.
`cache_bytes` bounds each reader's private sidecar page cache. Blocks can
require Immediate, and callers can declare automatic conditions; see
[Immediate policy](immediate-policy.md).
None of these values is a total file-size, total record-count, or lifetime
append limit.

The paged `.vks` backing file is deliberately not rejected by
`ResourceLimits::max_sidecar_len`: opening it does not materialize its full
length. That limit still applies to Varlit sidecar formats that are read into
memory, such as matrix/adapter envelopes. Stream sidecar safety instead
comes from bounded pages and private caches, fixed metadata rows, a fixed
number of block-tail rows, and checked native extents.

The runtime retains declared block tails and bounded buffers, not one RAM entry
per record.

## Scalar Append And The Bounded Sidecar Transaction

Single-record scalar `push_*` calls do not commit a sidecar transaction per
record. A stream writer keeps one open sidecar transaction and commits it only
at a chunk boundary — the configured transaction bound (`StreamOptions::state`
`.batch`, derived from `max_records` and `max_bytes`) — or on an explicit
`flush()`/`sync()`. Several scalar appends therefore share one sidecar commit.

The sidecar intentionally lags the native file: the native records are the
authority, and the sidecar carries only bounded resume state. Block-tail
maintenance for `keyed_offset_chain` formats uses binary search over the sorted
tail vector and collapses each chunk to one final tail per block, so tail
upkeep is `O(log blocks)` per record rather than linear.

## Explicit sidecar compaction

`writer.compact_index()` refuses while records appended since the last `sync()`
are pending (`DiskIndexError::CleanStateRequired`); call `sync()` or
`immediate()` first. When synced records are not yet covered by a confirmed
sidecar checkpoint, compaction confirms them first. It then copies the current
state into a bounded-memory temporary file, syncs it, atomically replaces the
sidecar, and syncs the parent directory. `IndexCompaction` reports the logical
sidecar size before and after. Old reader file descriptors remain valid;
`follow()` can adopt the replacement and subsequent writer generations.

This reclaims obsolete sidecar pages and checkpoints, not native log records.
Outside compaction, the writer reuses a sidecar page once it has been
unreferenced for `DiskIndexOptions::reclaim_after` confirmed generations
(default 2). A reader that follows at least that often never meets a reused
page; one that falls further behind gets a damaged-sidecar error and must
reopen. Crash safety does not depend on this setting.

A filesystem may retain the old file's allocated space until the final old
handle follows or closes; compaction requires enough free space for both files
while it runs. A `PublishedButParentSyncPending` error means the replacement is
visible but its name is not yet durably acknowledged. Compaction errors
conservatively poison the writer; close and reopen before more writes.

## Durability And Recovery

`flush()` commits the open sidecar chunk and flushes the native file handle;
it makes nothing durable and nothing visible to readers. `sync()` is the
durability and visibility boundary. It runs through the Kani-verified `commit`
driver in `crates/varlit-core/src/publication_protocol.rs`:

1. append a sync marker that closes the records since the previous sync;
2. `sync_all` the authoritative native file — the one storage barrier of an
   ordinary sync;
3. when the primary has grown past `StreamOptions::state.confirm` since the
   last confirmed checkpoint, write and confirm a sidecar checkpoint. If that
   publication fails after it may have become visible, the writer is poisoned.

A sync that does not confirm the sidecar is still durable and visible: readers
and recovery take the confirmed checkpoint and then walk the verified chain of
sync markers after it in the primary. `immediate()` is the same operation.

After any append, dropping the writer without a successful `sync()` leaves an
unsynced suffix. Reader open serves the confirmed checkpoint plus the synced
records after it, and never reads the unsynced suffix.

Both mirrored confirmed sidecar heads being unreadable is a corruption
error, including during explicit restore. Working/dirty heads cannot replace
the confirmed anchor: they may describe an older rollback point. Open/restore
refuse without truncating the primary or rewriting the damaged sidecar.
One valid confirmed copy still serves its snapshot, and already opened readers
keep their pinned confirmed generation. Preserve damaged files for inspection;
ordinary dirty-tail recovery does not repair lost confirmed anchors.

| Situation | Operation | Native scan |
| --- | --- | --- |
| `.vks` trails the primary (unconfirmed tail, interrupted generation) | `open_stream_writer` (recovers automatically) or `restore_stream_writer` | no prefix scan; walks the synced records after the checkpoint, cuts the unsynced suffix |
| native file has no `.vks` | `bootstrap_stream_checkpoint` | yes |
| `.vks` is stale or belongs to another primary generation | remove the `.vks`, then `bootstrap_stream_checkpoint` | yes |
| eager integrity check | reader `verify_all` | yes |

Recovery keeps every acknowledged sync and nothing after the last one. It
replays the kept records into the sidecar and confirms it.

Bootstrap uses the runtime `ResourceLimits` carried by its options and returns
scanned record/byte counts. `bootstrap_stream_checkpoint_with_progress` exposes
synchronous progress and cooperative cancellation. Bootstrap refuses to
overwrite any existing `.vks`, so an unsynced physical tail cannot be promoted
to clean merely by invoking a scan operation; remove a `.vks` only after
establishing that it is stale.

Sidecar protocol failures retain their public `DiskIndexError` source inside
`Error::DiskIndex`; sidecar lock contention is `Error::IndexBusy`. Callers can
distinguish dirty, stale, identity, metadata, and I/O failures without parsing
error text.

### Primary Generation Binding

A sidecar is bound to one logical *generation* of its primary, not merely to one
pathname or one OS file object. Every `VarlitStreamWriter::create` stamps a
128-bit random nonce as the primary's first record, under the reserved internal
block id `CREATION_NONCE_BLOCK_ID`. The sidecar records a primary-generation
witness that folds that nonce together with the primary's length and a checksum
over its bounded leading window (4 KiB; frozen once the primary has grown past
it, so appends do no witness work).

Every stream writer open, reader snapshot open, follow and checkpoint restore
recomputes the witness and refuses a mismatch with
`DiskIndexError::PrimaryGenerationMismatch`. This is what makes an in-place
rewrite of a primary by another equal-length primary of the same format — same
path, same OS object, same header bytes, same schema hash — a refusal rather
than an accepted stale generation. The recovery is to remove the `.vks` and run
`bootstrap_stream_checkpoint`, never "trust the sidecar".

One byte range of the primary is excluded from that window, and it is the only
part of a file that may change without being a new generation: a format that
declares `index: header_tails` reserves a region inside the file header which a
commit rewrites in place, and the witness blanks that region's *payload* before
checksumming. Its magic and its declared length stay covered, so a region that
changed size — the change that moves the end of the header and every record
offset after it — is still a refusal. For a format that does not declare the
region, the window is exactly what it was.

The nonce costs one record at create and nothing per append. Reading it is one
bounded point read of the leading record, performed only at create and open. A
primary that carries no nonce — a legacy file, or one bootstrapped from a
resident `VarlitFile` — is reported as "no nonce" rather than as an error, which
is fail-closed: a primary that *was* created with one recorded a witness that
folds it in, so answering "none" can only produce a different witness and refuse
the sidecar.

Sidecar metadata is at record version 3. A version 2 sidecar is 260 bytes where
the v3 record is 300, so it is refused with the typed
`DiskIndexError::MetadataLength` before the version field is read. Under the
pre-1.0 wire policy it is not migrated in place: remove it and run
`bootstrap_stream_checkpoint`. The `.vks` file uses the `VARVEIX5` paged
container; old redb-based sidecars likewise require bootstrap, and there is no
compatibility decoder.

Atomic sidecar publication syncs the parent directory after the
`ReplaceFileW`/rename (the Windows directory handle is opened with the write
access `FlushFileBuffers` requires). A parent-sync failure is never silently
promoted to full durability: sidecar create, stream bootstrap and sidecar
compaction surface `Error::PublishedButParentSyncPending` while keeping the
completed atomic replacement in place, so the published sidecar remains usable
and the pending state is a durability warning, not a rollback.

### Long Scan Progress And Cancellation

`verify_all_with_progress` and `bootstrap_stream_checkpoint_with_progress` use
the same scan-control types:

```rust
# fn scan(reader: &varlit::VarlitStreamReader) -> varlit::Result<()> {
use std::num::NonZeroU64;
use varlit::{
    ScanCancellationToken, ScanOptions, ScanProgressOptions, ScanProgressPhase,
};

let cancellation = ScanCancellationToken::try_new()?;
let scan = ScanOptions {
    progress: ScanProgressOptions {
        every_records: NonZeroU64::new(100_000),
        every_bytes: NonZeroU64::new(64 * 1024 * 1024),
    },
    cancellation: Some(&cancellation),
};

let records = reader.verify_all_with_progress(scan, |progress| {
    if progress.phase == ScanProgressPhase::Running {
        eprintln!(
            "{} records, {} / {} bytes",
            progress.records,
            progress.current_offset,
            progress.snapshot_len,
        );
    }
})?;
# Ok::<(), varlit::Error>(())
# }
```

The default callback cadence is every 16,384 records or 16 MiB, whichever is
reached first. Setting both cadence fields to `None` emits only `Started` and
`Complete`. `current_offset` and `snapshot_len` are absolute native offsets;
`scanned_bytes` excludes the file header. Values are monotonic and callbacks
run on the scanning thread. Display absolute completion with
`current_offset / snapshot_len`; use `scanned_bytes` for scan-throughput
accounting rather than mixing the two coordinate systems.

`try_new` reports token allocation refusal. Existing `new`/`default` and token
cloning remain available; all clones share the same cancellation flag.

Cancellation is checked before scanning, after every completed record, after
every callback, and after the final `Complete` callback. It returns
`Error::ScanCancelled { progress }`. A cancellation observed before that final
check publishes no `.vks`; a request arriving afterward may race with a
successful publication.

### Clearing A Stale Writer Lock

Default create/open/recovery remains single-writer and refuses any existing
lock. After independently establishing that the recorded process has exited,
the generated format exposes an O(1) lock-only recovery operation:

```rust
use varlit::WriterLockBreakPolicy;

Capture::clear_stale_writer_lock(
    "capture.varlit",
    WriterLockBreakPolicy::BreakIfProcessAbsent,
)?;
# Ok::<(), varlit::Error>(())
```

This operation does not open or scan the native data file. Every cooperating
writer first acquires an OS-level exclusive byte-range lock outside the bounded
metadata region. Recovery must acquire the same guard before it inspects or
clears stale metadata, so an active or concurrent writer cannot be displaced.
`Refuse` remains the default. Age-only policies are explicit operator choices,
but they are evaluated only after the exclusive guard is held; prefer
`BreakIfProcessAbsent` when process state is available.

Clean close and successful recovery clear the metadata while holding the
guard. A zero-length `.lock` file may remain as the stable filesystem identity
used by future guard acquisitions; `inspect_writer_lock()` reports it as no
active or stale marker.

## Per-thread readers and private caches

Each stream, resident and matrix reader belongs to one reading thread:
these handles and generated wrappers are `Send` but not `Sync`. Moving ownership to a worker is
supported; sharing one reader through `Arc` or `&Reader` across threads fails to
compile. Open a fresh reader on the same path for each reading thread. The
underlying files are common; each reader's caches and cursor state remain
independent. There is no implicit reader clone or shared-reader mutex.

A matrix reader also owns its matrix state: a fixed-size table of resolved
tile positions, sized at open from the declared extents, and an overlay of the
ranges published since the last checkpoint, which grows with the publications
the write-ahead ring holds. It keeps no commit-bitmap cache. Memory adds
across readers; see
[concurrency limitations](known-limitations.md#42-where-a-reader-gets-a-snapshot-rather-than-live-state).

Ordinary queries (`get`, `lookup`, `blocks`, `events`, `verify_all`) take `&self`.
`follow(&mut self)` updates only that handle's confirmed generation, and iterator
`next(&mut self)` advances only its cursor. Neither requires a writable primary
file or a writer transaction.

Matrix follow reads the write-ahead ring from where the reader's snapshot
ends and adds the newly published ranges to its overlay; when a checkpoint has
reclaimed ring space past the snapshot, it rebuilds the overlay from what the
ring still holds. Ranges already in place after a checkpoint leave the overlay.
Follow does not write and does not checkpoint. See
[durability](durability-model.md#what-readers-see).

Each stream reader opens the sidecar read-only and each snapshot owns its
bounded page cache. A sole writer appends sidecar pages and checkpoint records.
Two alternating confirmed heads are separate from the working heads and durable
Dirty marker. Readers validate CRC-protected confirmed heads, then read only the
state and native EOF they name, plus the verified sync-marker chain after that
EOF. Unconfirmed sidecar pages never replace a reader's state.
Reader open performs no write, transaction creation, OS lock or writer admission.
The same contract works across threads and processes on a coherent local filesystem.

`follow(&mut self)` adopts the latest confirmed sidecar generation and the
synced records after it, and returns the number of additional native records
(including internal records and tombstones). It runs through the Kani-verified
`follow` driver in `crates/varlit-core/src/publication_protocol.rs`: every
fallible step — capturing the sidecar state, validating EOF and the generation
witness on the retained native file object, walking the new sync markers —
reads the handle without changing it, and only an infallible step installs the
result. A failed follow therefore leaves the reader unchanged. A sidecar-only
`compact_index()` replacement is followed after identity and frontier
validation. Replacement of the native file itself is not followed.

Existing iterators keep their captured view. `follow_events(&mut events)` and
`follow_blocks(&mut blocks)` extend a cursor created by the same reader and
resume from its existing offset, including after EOF. They preserve sequence
validation and cumulative read limits, without reopening or scanning old records.
A cursor that returned an error cannot be resumed. A separately opened reader's
cursor is rejected even if it names the same path.

```rust,ignore
let mut reader = MyFormat::open_stream_reader(path, options)?;
let mut cursor = reader.items()?;
loop {
    for item in cursor.by_ref() {
        consume(item?);
    }
    wait_for_application_notification();
    reader.follow_blocks(&mut cursor)?;
}
```

The single writer holds nonblocking native/sidecar OS writer guards. These
exclude a second writer; readers never acquire them. The store owns its pending
write batch and caches. Publication and restore require exclusive mutable
access; no atomic admission gate or cache-transfer queue is involved. No reader is required to exit, release or acknowledge a
checkpoint before the writer can append or publish.

Native data is synced before any sync marker or confirmed checkpoint that
covers it is relied on. Readers never read the writer's unsynced tail.

## Independent optional read checks

Choose read checks explicitly when the application controls its input files:

```rust
let options = varlit::StreamOptions {
    limits: varlit::ReadLimits::MISSING.with_read_validation(varlit::ReadValidation::TRUSTED),
    ..varlit::StreamOptions::default()
};
// MyFormat::open_stream_reader(path, options)?;
```

`FULL`, `CRC_ONLY`, `STRUCTURE_ONLY` and `TRUSTED` select the four combinations
of additional structure checks and data CRC. Leaving the override unset retains
existing behavior. Bounds, resource limits, decoder validity, traversal
termination and publication guards remain. A well-formed wrong value need not
produce an error when its detecting check is disabled. Stored checksum policy
and write behavior remain unchanged, and CRC-on requires a format that stores
CRCs.

This does not remove the initial framing needed by a stream/demand directory,
or protocol/generation checksums. There is no unbounded cache or larger
resource allowance behind the option.
See the [complete contract and baseline review](design/read-validation-policy-review.md)
for exact I/O scope, explicit verification, recovery and mmap restrictions.

## Native follow and boundary-cache reuse

Native handles retain the last complete pending frame and staging storage.
With a cooperating current writer, unchanged polls frame no old records, and
growth frames only newly complete records. A commit discovered from cached
progress causes one authoritative replay of that pending batch before the
reader publishes its new view. Cached progress alone cannot expose pending data.

Writer/lease changes, truncation and incompatible state invalidate progress.
Legacy or unknown writers use conservative growth handling. Initial opens,
explicit verification and invalidated state can still require scans; this is
not a constant-time guarantee for every follow or mark.

Full scans and successful appends can retain validated nonresident record
offsets, eight bytes each. Their buffers share at most 16 KiB of the existing
directory allowance, with larger metadata spilled to private storage. Failure
to allocate or spill this optional acceleration falls back to checked traversal.
Sequence hints only select a candidate; its actual offset must match before a
native mutation is authorized. Full record metadata remains nonresident.

## Checked I/O Boundary

Sidecar offsets and lengths are untrusted. They become private-field
`FileOffset`, `ByteLength`, `SnapshotBounds`, and validated record-pointer
types before positional I/O or allocation. Validation uses checked `u64`
arithmetic and always rejects overflow and extents beyond the pinned EOF.
Required parsing, traversal budgets and publication checks remain in every
read mode. Additional framing/identity comparisons and data CRC verification
follow the selected read policy described above. Sidecar writers, recovery and
bootstrap keep strict metadata admission.

This prevents a malicious file from turning a claimed extent into an unchecked
slice or claim-sized allocation. It does not authenticate a file. CRC is an
integrity/error-detection policy, not a cryptographic authenticity mechanism.

## Snapshot retention management

Stream readers expose `snapshot_status()` with the adopted and latest
confirmed generations, record/byte lag, and whether their backend snapshot is
pinned. Each reader owns its file snapshot; there is no process-global reader
registry or `snapshot_retention()` API. Applications that need a reader count or
oldest active generation track their own reader lifetimes and reported status.
Old open files remain readable after compaction until their handles are dropped.
`IndexCompaction` reports the old and new logical sidecar sizes.

Call `release_snapshot(&mut self)` on an idle reader to drop its backend page
reference immediately. It returns true once and false if already released.
Native scans and existing cursors retain their EOF and remain usable;
they do not pin sidecar pages. `follow()` reacquires the latest confirmed snapshot,
even if its generation is unchanged. On failure, the reader remains released
and its previous native frontier is unchanged.

A typical policy is to report lag, ask the owning reader thread to `follow()`
when it may advance, or release its snapshot while idle. The writer cannot
forcibly advance or invalidate another thread's snapshot.
Dropping or following a reader releases its old snapshot pin. Open reader
handles can retain an old file descriptor even after releasing a snapshot. Follow or close
the old handle to release its file descriptor after file-generation compaction.
None of these APIs commits, fsyncs, changes Immediate conditions, or waits for
readers to catch up.

### Independent reader startup

Native reader opens do not compete for writer admission or return `IndexBusy`
for a live writer. If an external replacement or unsupported filesystem reports
a transient sharing/open error, retry with a bounded backoff/deadline on the
reader thread. Never gate writer progress on reader startup or retries.

Event and typed cursors are local to their creating thread (`!Send + !Sync`).
Move a `Send + !Sync` reader first, then create its cursors on the reading thread.
