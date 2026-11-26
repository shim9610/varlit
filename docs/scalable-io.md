# Scalable Stream I/O

This guide covers the stream handles of the default public API. They are the
Varlit path intended for very large append logs. Their normal open, append and
follow paths do not rescan the complete native prefix. A stream file carries
its durable boundary itself: each `sync()` writes the end of what it made
durable into the file's `VPUB` header block, and the writer's resume state
(record count, next sequence and one tail per declared block) is carried in
the stream summary of the file's anchors. There is no companion file; a
`<file>.vks` left by an earlier release is ignored. Resident buffers are
bounded; publication frequency and the number of declared blocks still affect
publication work.

> **Keyed lookup.** Stream readers have no lookup by key. The indexed primary
> (`VarlitIndexedWriter`, `VarlitIndexedReader`, the `.vki` sidecar and the
> `key_index` block attribute) was removed: updating keyed records grew its
> files without bound and it could never reclaim superseded versions. Lookup
> by key is available on the native handles, opt-in, through
> `keyed_storage: in_place` ([keyed storage](api-reference.md#keyed-storage)):
> one record per key in an in-file B-tree updated in place. Every stream
> constructor refuses a keyed-storage format with `StreamingUnsupported`. See
> the [migration guide](migration-guide.md) for existing indexed files.

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
> Whole-native-log keyed merge/compact uses bounded buffers and private spill
> files.
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

Opening the reader does not *scan* the native record region. It reads the
published end from the file header, then walks forward from there over any
syncs whose end was not yet published (normally none, so a clean open frames
nothing; see [Durability And Recovery](#durability-and-recovery)). Calling
`runs`, `frames`, `events`, or `verify_all` explicitly creates a native
scanner; their cost is linear in the selected snapshot.

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

`StreamOptions` has one field, `limits`, the runtime `ResourceLimits` the
handle enforces. Blocks can require Immediate, and callers can declare
automatic conditions; see [Immediate policy](immediate-policy.md).
None of these values is a total file-size, total record-count, or lifetime
append limit.

The runtime retains declared block tails and bounded buffers, not one RAM entry
per record.

## Scalar Append

A single-record scalar `push_*` call encodes its record into the writer's
reusable record buffer and writes it to the native file. It is not visible to
readers until the next `sync()` (see below). Block-tail maintenance for
`keyed_offset_chain` formats uses binary search over the sorted tail vector
and collapses each chunk to one final tail per block, so tail upkeep is
`O(log blocks)` per record rather than linear.

## Durability And Recovery

`flush()` flushes the native file handle; it makes nothing durable and nothing
visible to readers. `sync()` is the durability and visibility boundary. It runs
through the Kani-verified `commit` driver in
`crates/varlit-core/src/publication_protocol.rs`:

1. append a sync marker that closes the records since the previous sync (a
   sync with nothing new appends none);
2. `sync_all` the native file — the one storage barrier of a sync;
3. write the new end into the `VPUB` slot after the newest one: one 24-byte
   positional write, not synced on its own. The next barrier makes it durable.

The third step is best-effort. A failed or lost slot write leaves the earlier
end, which only lengthens the next open's marker walk: the records are already
durable and their marker verifies. `immediate()` is `sync()`.

`create` writes the creation nonce record and, for an embedded manifest, the
manifest record, publishes their end as the start of the marker chain, syncs
the file and then syncs its parent directory. A failure of that last step is
`Error::PublishedButParentSyncPending` naming the stream file: the file is
complete and a plain `open` resumes it, but its directory entry is not yet
proved durable.

After any append, dropping the writer without a successful `sync()` leaves an
unsynced suffix. Readers never read it.

### What readers and writers open

The `VPUB` block holds two 24-byte checksummed slots, written alternately (see
the [specification](spec.md#published-end)). A torn slot write fails its
checksum and leaves the other slot, which names an earlier end.

A reader open reads the published end first and measures the file after it, so
an end published between the two reads is never mistaken for lost records. It
then walks the sync-marker chain forward from that end and exposes exactly the
prefix through the last marker that verifies. A published end beyond the file,
or before the end of its header, is `Error::CorruptTail`: acknowledged records
were lost.

A writer open recovers automatically; there is no separate restore operation.
It takes the state at the published end (next sequence, record count, block
tails) from the stream summary carried by the newest intact anchor before that
end, trying at most four anchors, newest first, and frames the records from
that anchor's spanning record to the end. With no usable anchor it frames from
the header. Anchors are written at `AnchorPolicy` spacing, 4 MiB by default, so
this framing is bounded by the spacing, not by the file size. A record before
the published end that does not frame is `Error::CorruptTail`. The writer then
keeps every later sync whose marker verifies, cuts everything after the last
verified marker, syncs, publishes the kept end and starts a new marker chain
there. The cut, sync and publish run in that order through the Kani-verified
`restore` driver.

Recovery keeps every acknowledged sync and nothing after the last one. Only
the sync in flight at a crash can be torn, so a marker that does not verify
followed by a later marker is corruption, `Error::ChecksumMismatch`, and is not
cut away as a torn tail.

| Situation | What open does |
| --- | --- |
| clean file, end published by the last sync | reader frames nothing; writer frames from the newest anchor before the end |
| crash after a barrier, before its end was published or made durable | reader and writer walk the markers after the published end and keep each sync that verifies |
| unsynced suffix after the last verified marker | reader ignores it; writer cuts it, syncs and publishes |
| no published end (file written before `VPUB`, or both slots unreadable) | framed from the header, see below |
| eager integrity check | reader `verify_all` (full scan) |

A file with no published end is framed from its header. In a file written
before the `VPUB` block existed, the marker chain is taken to begin at the
link start of the first sync marker (where the former state sidecar's chain
began); a file with no marker, or whose first link does not start on a record
boundary, keeps every record that frames. Such a file has no `VPUB` block, so
every open frames it from the header; rewrite a large old stream with this
release if open time matters. A `.vks` file beside it is ignored and can be
deleted.

A native `VarlitFile` writer of a format without transaction markers bounds no
reader: when it takes over a stream file it writes an inactive slot,
withdrawing the published end. The next open then keeps every record that
frames, and a stream writer open publishes the end again. The native records
carry no sync marker, so a stream reader that was already open does not adopt
them with `follow()` until a stream writer has reopened the file and published
their end.

### Long Scan Progress And Cancellation

`verify_all_with_progress` uses the scan-control types:

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
`Error::ScanCancelled { progress }`. A verification scan writes nothing, so a
cancelled one leaves the file and the reader as they were.

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
ranges published since the last checkpoint. The overlay is a fixed-capacity
table allocated at open (one range per 256 ring bytes, at least 256 ranges); a
writer whose publication would need more moves the rest into an index, so every
snapshot fits. It keeps no commit-bitmap cache. Memory adds
across readers; see
[concurrency limitations](known-limitations.md#42-where-a-reader-gets-a-snapshot-rather-than-live-state).

Ordinary queries (`get`, `lookup`, `blocks`, `events`, `verify_all`) take `&self`.
`follow(&mut self)` updates only that handle's confirmed prefix, and iterator
`next(&mut self)` advances only its cursor. Neither requires a writable primary
file or a writer transaction.

Matrix follow reads the write-ahead ring from where the reader's snapshot
ends and adds the newly published ranges to its overlay; when a checkpoint has
reclaimed ring space past the snapshot, it rebuilds the overlay from what the
ring still holds. Ranges already in place after a checkpoint leave the overlay.
Follow does not write and does not checkpoint. See
[durability](durability-model.md#what-readers-see).

Each stream reader opens the native file read-only. A sole writer appends
records and sync markers and rewrites only the `VPUB` slots in place. Readers
read the published end, then only the verified sync-marker chain after it;
records past the last verified marker never enter a reader's view.
Reader open performs no write, transaction creation, OS lock or writer admission.
The same contract works across threads and processes on a coherent local filesystem.

`follow(&mut self)` re-reads the published end, continues the marker walk from
where the reader stopped, and frames only the newly confirmed records to count
them. It returns the number of additional native records (including internal
records and tombstones). It runs through the Kani-verified `follow` driver in
`crates/varlit-core/src/publication_protocol.rs`: every fallible step —
reading the published end, measuring the file, walking the new sync markers,
framing the new records — reads the handle without changing it, and only an
infallible step installs the result. A failed follow therefore leaves the
reader unchanged. Replacement of the native file itself is not followed.

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

The single writer holds nonblocking native OS writer guards. These exclude a
second writer; readers never acquire them. Publication and recovery require
exclusive mutable access; no atomic admission gate or cache-transfer queue is
involved. No reader is required to exit, release or acknowledge anything before
the writer can append or publish.

A published end names only data that a completed barrier made durable. Readers
never read the writer's unsynced tail.

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

Offsets and lengths read from a file — published ends, sync-marker links,
anchor summaries, record headers — are untrusted. They become private-field
`FileOffset`, `ByteLength`, `SnapshotBounds`, and validated record-pointer
types before positional I/O or allocation. Validation uses checked `u64`
arithmetic and always rejects overflow and extents beyond the pinned EOF.
Required parsing, traversal budgets and publication checks remain in every
read mode. Additional framing/identity comparisons and data CRC verification
follow the selected read policy described above. Writers and recovery keep
strict metadata admission.

This prevents a malicious file from turning a claimed extent into an unchecked
slice or claim-sized allocation. It does not authenticate a file. CRC is an
integrity/error-detection policy, not a cryptographic authenticity mechanism.

## Reader lag

Stream readers expose `snapshot_status()`, which returns a `SnapshotStatus`:

| Field | Meaning |
| --- | --- |
| `committed_len` | end of the prefix this reader holds |
| `records_behind` | confirmed records past it |
| `bytes_behind` | confirmed bytes past it |

It reports what `follow()` would adopt now: it reads the published end, walks
the markers after the reader's end and frames only the records past it. It
changes nothing. Each reader owns its file snapshot; there is no process-global
reader registry or `snapshot_retention()` API. Applications that need a reader
count track their own reader lifetimes and reported status.

A typical policy is to report lag and ask the owning reader thread to
`follow()` when it may advance. The writer cannot forcibly advance or
invalidate another thread's snapshot, and a reader that falls behind holds
nothing the writer needs. None of these APIs commits, fsyncs, changes
Immediate conditions, or waits for readers to catch up.

### Independent reader startup

Stream and native reader opens do not compete for writer admission and do not
fail because a writer is live. If an external replacement or unsupported filesystem reports
a transient sharing/open error, retry with a bounded backoff/deadline on the
reader thread. Never gate writer progress on reader startup or retries.

Event and typed cursors are local to their creating thread (`!Send + !Sync`).
Move a `Send + !Sync` reader first, then create its cursors on the reading thread.
