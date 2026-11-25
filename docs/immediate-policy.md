# Immediate policy for append writers

An Immediate boundary makes all preceding writes, including its triggering
record, durable before returning success. What it means for readers depends on
the writer:

- A native writer (`VarlitFile`, `VarlitWriter` and the generated native
  wrappers) makes each write visible to readers at once. Its Immediate
  boundaries decide only what is synced.
- A stream writer (`VarlitStreamWriter` and the generated stream wrapper)
  makes records visible to readers only at a durability boundary, so each
  Immediate boundary is also a publication. It also confirms the sidecar
  checkpoint when the writer's `DiskIndexConfirmPolicy` is due.

There are three ways to request one.

## Block declarations

```rust,ignore
varlit_format! {
    pub format Journal {
        magic: b"JRNL";
        version: 1;
        schema_hash: computed;
        index: keyed_offset_chain;
        blocks {
            variable Sample(id = 1, key = [id]) {
                id: u64,
                bytes: Vec<u8>,
            }
            fixed Checkpoint(id = 2, durability = immediate) {
                position: u64,
            }
            fixed Status(id = 3, immediate_if = final_status) {
                complete: bool,
            }
        }
    }
}

fn final_status(status: &Status) -> bool { status.complete }
```

Each `Checkpoint` append immediately persists itself and preceding samples.
In this `keyed_offset_chain` format the stream writer generates no method for
the keyed `Sample` block (it keeps no previous-key chain), so `Sample` is written
through a native writer; the unkeyed blocks work on both.
The same rule applies to deletions of an Immediate keyed block. `immediate_if`
receives an appended value; it does not run on a deletion, which has only a key.
Both declarations are OR-combined with any runtime policy; runtime settings
cannot disable a mandatory block boundary. Unannotated blocks are deferred.

For independently derived blocks, use `#[varlit(durability = "immediate")]`
and `#[varlit(immediate_if = "final_status")]`. Manual `VarlitBlock` implementations
can override `const IMMEDIATE: bool` and `fn immediate_if(&self) -> bool`.
These are writer policies, not serialized schema or file-format changes.
Use pure predicates: a batch can evaluate a condition before its native write.

## Runtime conditions and explicit calls

```rust,ignore
writer.set_immediate_policy(
    ImmediatePolicy::new()
        .after_records(16_384)
        .after_bytes(64 * 1024 * 1024)
        .when(|event| event.block_id == 2)
)?;

writer.push_sample(&sample)?;
writer.immediate()?;
```

The conditions are OR-combined. `when` accepts a function pointer or a
non-capturing closure over `ImmediateEvent` (block id, operation, sequence,
record size, pending record count and pending native bytes). A delete event
names its user block, not the internal tombstone block. Byte accounting includes
record headers and footers. Thresholds include the triggering record; a large
record can exceed a byte threshold. Zero thresholds fail installation without
replacing the existing policy.

Counters cover the caller's appends/deletions on this handle and reset only
after successful `sync()` or `immediate()` (`commit_durable()` also resets
native-writer counters). A stream writer's counters start at the first caller
write: the creation nonce and embedded manifest records that `create` writes do
not count.
Changing policy neither resets counters nor performs I/O. The next mutation
evaluates the new policy. `ImmediatePolicy::default()` removes runtime conditions.
There is no background timer and no I/O from a condition declaration itself.

Within `push_*s` / `push_iter`, an Immediate condition drains the current native
prefix and completes the durability boundary **before consuming the next input
value**. A declared Immediate block therefore requests a boundary per record,
even when passed through a plural/batch API. Use a separate checkpoint block or
a record/byte condition to select a larger durability interval.

The APIs are available on `VarlitFile`, `VarlitWriter`, `VarlitStreamWriter`,
and their generated append writer wrappers. Stream writers are included by
default. Conditions govern typed
user appends and deletions; matrix cells, in-place replacement, metadata and
merge-op APIs retain their existing contracts. Matrix block declarations reject
these attributes: use the matrix ordered-barrier API.

On a native format with transaction markers, `immediate()` syncs bytes but does
not implicitly create a logical commit marker. Use `commit_durable()` when the
transaction itself must become committed and durable.

## Stream commit and recovery order

1. Before the first mutation after a clean boundary, save the clean rollback
   checkpoint and mark the generation Dirty. This marker is not a barrier: a
   crash that loses it leaves the newest durable state clean with the native
   file longer than it covers, which is recovered by restore exactly as a
   surviving Dirty checkpoint is.
2. Write native chunks and append bounded sidecar working checkpoints without
   a durability barrier. A native chunk does not force a sidecar commit; sidecar
   record/byte bounds, API-end publication and explicit boundaries do.
3. At `immediate()` / `sync()`, append a sync marker to the native file and
   sync it. This runs through the Kani-verified `commit` driver in
   `crates/varlit-core/src/publication_protocol.rs`. The sidecar is confirmed in the same call only when the native file
   has grown past the last confirmed EOF by the writer's
   `DiskIndexConfirmPolicy` (default 64 MiB or 65,536 records; `EVERY_SYNC`
   confirms on every sync). Until then readers and recovery take the unconfirmed
   tail from the native file. A confirmation finishes the pending sidecar batch,
   writes the clean checkpoint and syncs it, writes one confirmed head into the
   slot that does not hold the newest durable head, and syncs again. The head is
   then copied into the other slot without a barrier of its own; the next
   barrier persists it, so a completed generation keeps two head copies.
   A version 5 sidecar (`VIXHEAD2` heads) confirms with one sync instead: its
   head names the checksum of every sidecar byte appended since the previous
   confirmed checkpoint, a head whose generation fails that checksum is skipped
   for the other confirmed slot, and a generation whose two head copies both
   name it but fail the checksum is refused as damaged durable data.

A `sync()` therefore costs one storage barrier on the native file, plus two on
the sidecar when it confirms (one for a version 5 sidecar). A writer open adds
one sidecar sync when the two confirmed heads differ.

`flush()` does not request a new durable clean generation. Creating or
bootstrapping a sidecar and establishing a rollback checkpoint still perform
required safety I/O. This is not a promise of zero fsync calls before the first application
boundary. Neither Drop nor a working checkpoint is an application durability
acknowledgment.

Recovery restores the newest confirmed checkpoint, keeps every synced record
after it, and discards the unsynced suffix. Fresh readers open the confirmed
checkpoint plus the synced records after it. Existing readers adopt new
generations and synced records with `follow()`, which runs through the
Kani-verified `follow` driver; use
`follow_events` or `follow_blocks` to resume a parser without replaying its prefix.
Independent readers can run in separate processes. These paths add no mutex or
reader write-admission gate.

If an automatic boundary fails after append, the call returns
`Error::AppendedButImmediateFailed { sequence, source }`. Do not append the same
record again. The batch error's `written` prefix includes records already in the
native file, including the triggering record. If the writer remains usable,
retry `immediate`; a poisoned writer requires recovery. A direct `immediate()`
returns its original sync/sidecar error because it appends no new user record.

The batch and private-cache limits bound sidecar residency, not on-disk history.
Explicit `compact_index()` after a sync rewrites the sidecar to its current
state. Old open readers can retain the replaced file. Select record/byte
conditions for a finite durability interval and manage disk space explicitly.

## Validation

`immediate_policy` covers exact in-batch boundaries on stream writers, scalar
appends, native-writer deletes, threshold/predicate behavior, failed-boundary prefix reporting,
pinned readers, and subprocess exits without running writer destructors.
`immediate_native` exercises the public native wrappers without scalable features.
The existing crash matrix, reader-open churn and seeded histories remain
required by `scripts/qualify_scalable.py`.
