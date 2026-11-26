# Immediate policy for append writers

An Immediate boundary makes all preceding writes, including its triggering
record, durable before returning success. What it means for readers depends on
the writer:

- A native writer (`VarlitFile`, `VarlitWriter` and the generated native
  wrappers) makes each write visible to readers at once. Its Immediate
  boundaries decide only what is synced.
- A stream writer (`VarlitStreamWriter` and the generated stream wrapper)
  makes records visible to readers only at a durability boundary, so each
  Immediate boundary is also a publication: `immediate()` is `sync()`.

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

1. Write native chunks without a durability barrier. They are in the file but
   not visible to readers.
2. At `immediate()` / `sync()`, append a sync marker that closes the records
   since the previous sync and sync the native file. This runs through the
   Kani-verified `commit` driver in
   `crates/varlit-core/src/publication_protocol.rs`.
3. Write the new end into the file's `VPUB` header block: one 24-byte write
   into the slot after the newest, not synced on its own. The next barrier
   makes it durable; until then a crash leaves the earlier end, and the next
   open walks the verified markers after it.

A `sync()` therefore costs one storage barrier. `create` also syncs the file
and its parent directory once, and a writer open that cut an unsynced suffix
or found syncs past the published end syncs once before its first append.

`flush()` does not request a durable boundary. This is not a promise of zero
fsync calls before the first application boundary. Drop is not an application
durability acknowledgment.

Recovery is part of every writer open: it keeps every synced record whose
marker verifies and discards the unsynced suffix. Fresh readers open at the
published end plus the verified syncs after it. Existing readers adopt new
syncs with `follow()`, which runs through the Kani-verified `follow` driver; use
`follow_events` or `follow_blocks` to resume a parser without replaying its prefix.
Independent readers can run in separate processes. These paths add no mutex or
reader write-admission gate.

If an automatic boundary fails after append, the call returns
`Error::AppendedButImmediateFailed { sequence, source }`. Do not append the same
record again. The batch error's `written` prefix includes records already in the
native file, including the triggering record. If the writer remains usable,
retry `immediate`; a poisoned writer requires recovery. A direct `immediate()`
returns its original sync error because it appends no new user record.

The batch limits bound buffering, not on-disk history. Select record/byte
conditions for a finite durability interval and manage disk space explicitly.

## Validation

`immediate_policy` covers exact in-batch boundaries on stream writers, scalar
appends, native-writer deletes, threshold/predicate behavior, failed-boundary prefix reporting,
pinned readers, and subprocess exits without running writer destructors.
`immediate_native` exercises the public native wrappers without scalable features.
The existing crash matrix, reader-open churn and seeded histories remain
required by `scripts/qualify_scalable.py`.
