# Matrix Durability Model

This document defines ordered durability for matrix writes and the boundary for
post-commit hooks.

Append writers also expose [Immediate policies](immediate-policy.md): block
attributes, automatic conditions, and an explicit `immediate()` boundary. These
do not change the matrix ordered-barrier or transaction-marker visibility rules.

## Default Policy

Without an ordered barrier policy, matrix writers follow the existing Varlit
durability model:

- writes may be buffered
- `flush` pushes buffered bytes to the operating system
- `sync` requests durable persistence
- no per-cell fsync is implied
- `sync` (and `commit_durable`) additionally makes a pathname *created by that
  handle* durable — see "Created Pathname Durability" below
- a returned write may be visible through the current writer handle before it is
  crash-durable

Safe append-log fixed replacement is copy-on-write. The replacement generation
is written in the target directory, flushed and synced, fully reopened and
validated, then atomically published. Publication failure leaves the original
path generation unchanged, with the single Windows exception of the typed
indeterminate state described below. An already-open reader remains bound to its retained
file object and captured logical EOF rather than reopening the pathname.
If publication succeeds but the writer cannot reopen the published pathname,
Varlit returns `PublishedButRebindFailed` and poisons that writer. This is not a
publication rollback: callers must reopen and reconcile instead of blindly
retrying the operation. When the parent-directory sync for that same
publication also failed, the variant carries both facts — see "Two Durability
Facts, One Result" below.

## Commit Marker Versus Commit Durability

`commit_durable` does two things in order: it appends the transaction's commit
marker, and it then asks the operating system to make the file durable. The
first is the authoritative commit — once the marker bytes are in the file, a
reader that opens the file after a clean process exit sees the transaction as
committed, whether or not the durability request that follows succeeded.

Those two outcomes are therefore reported separately rather than collapsed into
one `Err`:

- a failure **before** the marker is appended returns an ordinary error and the
  transaction is not committed. Re-running it is correct;
- a failure of the `flush`/`sync_all` **after** the marker is appended returns
  `Error::CommittedButDurabilityUnproven { sequence, source }`. The transaction
  *is* committed and `sequence` names the marker record. What is unproven is
  only that it survives a power loss. The correct response is to retry `sync`;
  re-running the transaction appends a second marker for work that is already
  recorded;
- a failure of the created pathname's directory sync after the marker returns
  `Error::PublishedButParentSyncPending`, described in the next section, and
  leaves the request pending so a later `sync` retries it.

### What readers see before the barrier returns

Under a transaction-marker commit policy a reader, opened or following,
adopts a commit only once the writer has *published* it, and a durable
commit is published after its barrier returns. The writer's records and the
marker reach the file with positional writes before the barrier, so without
this a follower could adopt a transaction that a power loss would then take
away. The published end is a `VPUB` header block (see the
[specification](spec.md#published-end)) that the writer rewrites in place:

- `commit_durable` and `sync` publish after their barrier returns;
- `commit`, `flush` (with `OnFlush` markers) and releasing the writer promise
  visibility without durability, and publish at once;
- a read-write open syncs what an earlier writer left and publishes it.

Publishing costs one 24-byte positional write and no barrier. A reader stops
at the published end; bytes past it are not part of the file it sees. If the
barrier fails, the commit stays unpublished until a later `sync` succeeds
(`a_follower_does_not_adopt_a_commit_whose_barrier_did_not_return`); a clean
release then publishes it, as the paragraph above describes.

After an operating-system crash the newest published end may not have reached
the disk, since it is never synced on its own. A reader then stops at the end
published before it (the next barrier makes each end durable), which can hide
the last durable commit until a writer opens the file again and publishes
what it holds. It never shows a commit that was not durable.

Formats under other commit policies have no block: their records are visible
once written, by contract, and no visibility waits on a barrier.

This is the same published-outcome family as `PublishedButRebindFailed`,
`MatrixCommittedButHookFailed` and `MatrixCommittedButDurabilityUnproven`: a
caller that receives one of them must not treat the operation as
not-performed.

## Created Pathname Durability

Decision (DUR-01): `sync` and `commit_durable` establish the durability of a
pathname the calling handle created, in addition to the file's contents.

`sync_all` makes the file's bytes durable, but on platforms that require an
explicit directory sync it does not make the newly created *directory entry*
durable. A fully synced object with no surviving name does not deliver the
"durable persistence" the public contract promises, and the atomic replacement
path already syncs the parent directory and reports a pending parent sync — so a
create path that did not was internally inconsistent with the rest of the model.

The rule, precisely:

- the parent directory is fsynced on the **first** successful `sync` or
  `commit_durable` of a handle that created its pathname, and never again;
- it costs one directory fsync per created file. It is not per sync, not per
  commit, and never on the append path;
- a handle that merely **opened** an existing pathname does not re-establish it,
  because that name's durability is not this handle's to claim;
- if the platform refuses the directory sync, the result is the typed
  `Error::PublishedButParentSyncPending` rather than a silent success. The file
  contents are durable and the pathname is visible; only the directory entry's
  durability is unconfirmed, and the request stays pending so a later `sync`
  retries it.

This is a behaviour change: a first `sync` on a created file can now return
`PublishedButParentSyncPending` where it previously returned `Ok(())`.

## Single-Writer Lock On The Native File Object

The authoritative single-writer guard is an OS lock held on the open native
file *object*, not on a path-derived marker. On Windows Varlit takes an exclusive
`LockFileEx` (`LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY`) over a
one-byte range at a reserved offset that never overlaps real data, so ordinary
readers are unaffected. On Unix it takes an advisory exclusive lock on the same
handle. The guard is bound to freshly created files at create time and probed
for existing files at open time, and it is re-bound to each newly published
copy-on-write generation. Because the lock lives on the file object that every
name for the file shares, a second writer opening the file through a hard link
or reparse alias cannot acquire it — closing the path-alias bypass that the
former path-derived `.lock` scheme allowed.

The `<file>.lock` marker is retained only as diagnostic and break-policy
metadata (content is written below the reserved lock range). It is never the
authority for exclusion. `clear_stale_writer_lock` locks the marker object
first, then inspects it, and probes the target's native-object lock after that;
under the default `WriterLockBreakPolicy::Refuse` a marker that is present
refuses before the probe is reached at all. An active object lock still cannot
be displaced by recovery, because the marker is cleared only after the probe
succeeds and a live writer's hold makes the probe fail. A
zero-length `.lock` file may persist as a stable filesystem identity for future
guard acquisition; `inspect_writer_lock()` reports it as neither active nor
stale.

## Typed Post-Publication Replace Outcome

Atomic replacement renames or `ReplaceFileW`s the validated generation into
place and then syncs the parent directory. The rename is the publication point:
once it returns, the pathname already names the new generation. A parent-sync
error afterward therefore cannot mean "original unchanged". Varlit models the two
outcomes explicitly rather than letting a late error escape before rebinding:

- On a parent-sync failure after a successful rename, the writer is first
  re-bound to the published generation and then `PublishedButParentSyncPending`
  is returned. Publication happened; only power-loss durability of the new
  directory entry is unconfirmed. Callers may proceed or re-sync, but must not
  treat this as a rollback.
- If the post-publication re-bind itself fails, the writer is poisoned and
  `PublishedButRebindFailed` is returned.

### Two Durability Facts, One Result

Those two outcomes are not exclusive. A replacement learns two independent
facts in a fixed order — whether the parent-directory sync succeeded, and
whether the writer could rebind to the published generation — and only the
second one ends the call. When the sync fails, publication has already
happened, so the writer rebinds anyway and the caller normally learns about the
pending sync through `PublishedButParentSyncPending`. If the rebind then fails
too, that variant is never constructed.

Until F-07 the second failure simply discarded the first. The rebind variant is
now `PublishedButRebindFailed { sequence, source, parent_sync }`, where
`parent_sync: Option<Box<Error>>` carries the earlier sync failure, and its
`Display` renders both clauses:

- `parent_sync: None` — the new generation is visible at the pathname and the
  writer is unusable. The rename's own durability was established.
- `parent_sync: Some(_)` — the same two facts, **plus**: the rename is not yet
  proved durable against power loss, so a crash before the parent directory is
  flushed can leave the pathname naming the old generation. Reopen, reconcile,
  and sync.

The field is attached only to this variant, and only by the replacement paths
that actually observed the sync failure. Every other error from a replacement
path means publication did not happen, so a parent-sync fact would be
meaningless there. This is a source-breaking change for downstream code that
destructured the variant with `{ sequence, source }`; add `parent_sync` or
`..`.

On Windows, `ReplaceFileW` failures are classified per the documented OS
contract (`classify_replace_publication_error`, pure and unit-testable on every
platform):

- `ERROR_UNABLE_TO_REMOVE_REPLACED` (1175) is the documented no-mutation
  failure: the replaced file is intact under its original name.
- Every other error except the two below occurs before any mutation; both
  files retain their original names and the temp may be deleted.
- `ERROR_UNABLE_TO_MOVE_REPLACEMENT` (1176) and
  `ERROR_UNABLE_TO_MOVE_REPLACEMENT_2` (1177) can leave the two files' names,
  streams, and attributes partially moved. Varlit captures the replacement's OS
  object identity before the call and reconciles on 1176/1177: if the target
  pathname already resolves to the replacement object, the publication is
  treated as complete and proceeds through the normal rebind path. Otherwise
  Varlit returns the typed `Error::ReplacePublicationIndeterminate`: the
  replacement temp file is preserved for reconciliation, and a writer bound to
  the target is poisoned so a blind retry is impossible. Callers must inspect
  the pathname and the preserved temp separately before acting.

Any error from replacement other than these typed post-publication and
indeterminate states means publication did not happen and the original
generation still stands.

The parent-directory sync itself is honest about refusal. `FlushFileBuffers`
requires `GENERIC_WRITE` on the handle, so the Windows implementation opens the
parent directory with write access, and no open or flush failure is ever
promoted to a `Durable` result. On filesystems that refuse a directory
write-open or flush, every publication surfaces
`PublishedButParentSyncPending` rather than a false durability claim; the same
honesty applies to the native sidecar publication sites (stream sidecar
create, stream bootstrap and sidecar compaction), which surface the pending state
while preserving the already-published sidecar instead of silently discarding
it.

Exclusive-create constructors (`VarlitFile::create_new`,
`VarlitWriter::create_new`, and the matrix `create_new_with_dims` pair) open
with `create_new`, never truncating or reusing an existing path; they fail
with an `AlreadyExists` I/O error instead. All create paths bind the
single-writer object lock before destructive initialization: open without
truncate, bind the lock on the file object, then `set_len(0)` and write the
header, so a losing concurrent creator can never truncate the winner's freshly
initialized file inside the pre-bind window.

The unsafe exclusive in-place replacement method does not provide that snapshot
guarantee. Its safety contract requires process-wide and cross-process
exclusion, including mmap and raw references.

It carries a second requirement that F-02 made explicit: on a
`keyed_offset_chain` format, the caller must **not change the record's key**.
The entry point takes `T: VarlitBlock`, which exposes no key, so this cannot be
checked at that signature, and unlike the copy-on-write paths there is no new
generation in which the chain could be rebuilt — records appended after the
target already carry `prev_same_key_offset` pointers into it, and rewriting its
key in place would leave them linking a record that now claims a different key.
Ordinary keyed replacement enforces the same-key contract through
`VarlitReplaceBlock::validate_replacement`; here it is part of the unsafe
contract.

What the method guarantees regardless of whether that contract is honoured is
that no resident keyed-tail cache survives the mutation. The affected block's
tail map is dropped **before** the first byte is written, so a stale
predecessor can never be read afterwards — not after a write that fails
part-way, not through a poisoned writer that is later inspected, and not after
a contract violation. The invalidation is infallible and allocation-free, so it
adds no fallible step in either direction of the commit.

## Matrix Storage And Publication

A matrix lives inside the primary file. Its metadata record and a fixed-size
write-ahead ring are the first records of the append log. Each tile (the
`tile = [rows, cols]` shape a matrix block declares) is an internal record
appended the first time one of its cells is written, together with the radix
directory pages that locate it. After that, cell slots, tile commit bitmaps,
directory entries, single and per-channel commit flags and aux bytes are
rewritten in place. Overwrites do not grow the file
(`overwrites_keep_the_file_size_and_new_tiles_add_one_tile`,
`the_ring_holds_the_disk_space_constant`), except that a publication larger
than half the ring adds overflow records once, which later large publications
of the same writer reuse (see below). There is no companion file:
`matrix_generation_path()` returns the primary path, and copying the primary
copies the whole matrix. Primary-file replacement, rewrite and defragmentation
are refused for matrix files with `InvalidFormatSpec`. Stream handles refuse
a format that declares a matrix block with `StreamingUnsupported`; the indexed
primary that could carry a matrix was removed.

Readers trust only commit bits. A clear commit bit means `NotCommitted`
regardless of slot bytes. A slot holds the payload followed by its CRC32;
`write_matrix_cell()` stores the slot and clears the cell's commit bit, and
`commit_matrix_cell()` sets it.

### What a publication is

Matrix changes are staged in the writer. A change inside a tile appended since
the last publication is written in place at once, because nothing published
points at that tile yet; any other change is held in memory, and a staged set
larger than an eighth of the ring (at most 4 MiB) is written to the ring early
as frames that are not yet committed. A writer handle reads its own staged
changes; no other handle sees them.

`flush()` does not publish matrix changes. `sync()`, `immediate()`,
`commit_durable()` and the durable cell API publish them as the next
generation. A publication:

1. syncs the file first if it appended tiles or directory pages, so they are
   durable before a frame points at them;
2. writes the staged byte ranges to the ring as one or more frames, each with a
   CRC32, the last one marked as the commit frame;
3. syncs the file (`sync_data`);
4. writes a ring header naming the new generation into one of two alternating
   checksummed header slots.

Readers see the generation after step 4. That is one sync per publication, two
when the publication appended records
(`the_ring_holds_the_disk_space_constant` counts one barrier per publication
plus two per checkpoint). When the matrix has staged changes, `sync()` uses the
publication's sync as the file's barrier, so records appended before it become
durable through the same sync. The header write in step 4 is not synced on its
own: on reopen the frames, not the header, decide which generations were
published.

Under a transaction-marker commit policy, a publication that appended matrix
records first writes a commit marker when the log has records after the last
marker, so the matrix records are inside the committed prefix; that marker also
commits any user records appended since the previous marker. A publication
that appended nothing leaves uncommitted user records uncommitted, and a writer
reopen still discards them
(`matrix_only_sync_never_publishes_a_native_tail_that_recovery_will_truncate`).
With transaction-marker formats, `sync()` does not otherwise write a marker:
use `commit_durable()`, or `flush()` followed by `sync()` for `OnFlush` markers.

### The ring and checkpoints

The ring size is fixed when the matrix is created.
`FormatSpec::with_matrix_ring_bytes(bytes)` declares it; otherwise it is twice
the slot bytes of the declared extents, kept between 256 KiB and 16 MiB and no
larger than the `RecordPayloadLen` limit. A ring larger than 64 MiB is refused
with `LimitExceeded`. A frame holds at most a quarter of the ring.

### Publications larger than the ring

A publication is not limited by the ring, and its size never grows the
writer's memory. Every staged byte goes from the caller's buffer to the file
when it is staged: into a ring frame while the publication is small, and once
its frames would take more than half the ring (or a reader's overlay could
not hold its ranges, or one range does not fit a frame), into overflow space,
with an on-disk index (a B+tree keyed by target) recording where each range's
newest bytes are. The commit frame then names the index root: one 64-byte
frame whatever the publication's size. The commit is unchanged: the file,
index pages and overflow data included, is synced before the commit frame is
written, so readers see the whole publication or none of it
(`a_sync_larger_than_the_ring_publishes_whole_and_reuses_its_overflow`,
`a_crash_inside_a_streamed_publication_recovers_all_or_nothing`).

Readers look ranges up in the index through a fixed page cache; a checkpoint
walks the index in key order and writes each range in place through a fixed
copy buffer. Before a publication starts an index the writer puts every
published generation in place, so the overflow space earlier indexes used is
free again: repeating a large rewrite reuses the same regions instead of
growing the file. Overflow data is written over only after its generation is
checkpointed and the ring header records that no reader may still read it
(`reclaimed`); a reader that was behind gets `MatrixSnapshotExpired`, never
the newer bytes. Regions are whole mebibytes, each at least as long as what
its publication put in overflow space before it (so a long publication takes
a few regions of doubling size) and at most the record payload limit. The
metadata record points at a list of the regions, so a reopened writer finds
and reuses them too.

Free overflow space also takes new tiles and directory pages: one that fits
is placed at the end of a free region instead of being appended, and the
region's header records the space as taken with the publication that first
points at it. Rewriting a large part of the matrix and then extending it
therefore fills the space the rewrite used before the file grows
(`new_tiles_fill_free_overflow_space_and_the_space_survives_a_reopen`,
`a_crash_while_tiles_fill_overflow_space_recovers_all_or_nothing`).

### Memory

A matrix handle allocates what it needs when it is opened: the ring overlay
(`ring length / 256` ranges), a 256-page index cache, 64 KiB scan and copy
buffers, and cell, bitmap and key-order buffers sized by the format. After
that, writing and committing cells, publishing (through the ring or an
index), following, reading and checkpointing allocate nothing
(`matrix_data_paths_allocate_nothing_after_open`). New tiles, directory
pages and overflow regions are appended as records of zeros written from a
static page, without a buffer of their size; the file's in-memory record
index still grows by one entry for each such record.

When a frame does not fit, the writer runs a checkpoint inside that call. It
chooses the oldest published generations whose removal leaves room for the
frame and at least half the ring free, records in the ring header that the
file is about to hold values of those generations, writes the newest bytes of
each of their ranges in place, syncs, writes a header that moves the ring's
tail past them, and syncs again. A publication that needs a checkpoint
therefore costs two more syncs. Nothing runs in the background.

### What readers see

A read-only handle opens its matrix when the handle opens. Its snapshot is the
newest confirmed generation at that moment, one whose frames all verified and
whose commit frame is present: the file as of the last checkpoint, with an
overlay of the ranges published since, read from the ring.
Queries take `&self` and read that snapshot. Readers take no lock and do not
hold back the writer. Each reader is `Send + !Sync` and owns its caches.

`follow(&mut self)` adopts the newest published generation. Its return value
counts record bytes, so a matrix-only publication returns zero; compare
`matrix_generation()`, the number of the published generation the handle
shows, to observe it. A read-write handle's `follow()` returns zero and changes
nothing.

A checkpoint writes in place only generations up to its target, and it records
that target in the ring header before it writes. A reader whose snapshot is
older than that target can no longer read its snapshot from the file and the
ring, so its next read returns `MatrixSnapshotExpired { snapshot, checkpointed }`
instead of a mix of generations. `follow()` (or a reopen) moves it to the
newest generation. A reader expires only once a newer generation has been
published after its snapshot and the writer needs the ring space: for later
publications, or for staging that outgrows half the ring and starts an
overflow publication, which first puts the published generations in place
(`a_reader_that_falls_behind_the_ring_is_told_to_follow`,
`a_reader_keeps_its_snapshot_across_publications_and_checkpoints`). A reader
on the newest published generation does not expire however much is staged. An open or
`follow()` whose scan of the ring is overtaken by checkpoints 32 times in a row
also returns `MatrixSnapshotExpired`.

A read re-checks the ring header after it has read, and reads again (up to
eight times) if a checkpoint moved under it. A region page or a key batch does
this once for the whole call. A read returns values of the reader's snapshot
or `MatrixSnapshotExpired`, never a mix
(`a_page_never_mixes_generations`, `a_key_batch_never_mixes_generations`).

### Crash and reopen

Changes that were staged but not published are lost when the writer is dropped
or the process stops; dropping a writer does not publish
(`reopened_writer_discards_unpublished_matrix_changes`,
`unpublished_and_torn_publications_never_reach_a_reopen`).

Opening a writer reads the ring from the last checkpoint. A generation is
adopted only when all of its frames verify and its commit frame is present;
the scan stops at the first frame that is torn, out of sequence or fails its
CRC, so neither that generation nor any later one is adopted, and the next
publication overwrites its frames. Adopted generations become the writer's
overlay; nothing is moved in place at open, so a reader's snapshot stays
readable across a writer reopen. Each frame records the file length it depends
on; if recovery of the primary cut the file back to its last acknowledged sync
and below that length, the generation is not adopted, and the next publication
takes its number (`a_generation_needing_records_a_recovery_cut_off_is_not_adopted`).

A crash inside a publication leaves either the previous generation or the
whole new one (`a_crash_inside_a_publication_recovers_all_or_nothing`,
`a_crash_while_allocating_tiles_recovers_all_or_nothing`), and a crash inside a
checkpoint loses no published value
(`a_crash_inside_a_checkpoint_keeps_every_published_value`). These tests run
the engine on an in-memory store whose crash keeps the synced bytes plus an
in-order prefix of the later writes, with the next write torn. They do not
model reordered writes or a storage device's power loss.

`a_killed_writer_leaves_whole_publications` kills a real writer process 32
times at varying points and reopens the real file each time. Every cell must
show one round, the round must be the last acknowledged one or the one after
it, and it must never go back. A reader may show one round less than the next
writer adopts: a publication whose frames are complete in the ring but whose
header update did not happen is finished by that writer's recovery, after
which readers see it. A killed process loses nothing the kernel accepted, so
this checks partial sequences of system calls, not power loss.

The checksums detect accidental corruption, not deliberate rewriting. A ring
header or metadata record whose checksum holds but whose fields are impossible
(offsets outside the ring, unordered or saturated generations, a ring outside
the file) is refused with `InvalidMatrixLayout`
(`a_ring_header_with_impossible_fields_is_refused_not_followed`,
`a_ring_placed_outside_the_file_is_refused`). Tile directory pages carry no
checksum; a root, entry or height in one that cannot be a place in a file is
refused the same way
(`directory_offsets_that_are_not_file_offsets_are_refused`), but a directory
entry rewritten to another valid offset is not detected by the directory
itself: only `verify_matrix_metadata`, which checks each tile's header, sees it.

### Compaction

There is no matrix compaction; `compact_matrix()` was removed in 0.13.0.
Overwrites happen in place and the ring has a fixed size, so there is nothing
to reclaim. Tiles, once appended, are never removed.

## Append-Log Transaction Markers

Append-log transaction markers follow the same explicit durability principle.
`commit()` writes a logical visibility marker for
`transaction_marker(explicit)` formats, but it is not a hidden fsync barrier.
Use `commit_durable()` when the marker itself must only be published after
commit-covered records, embedded manifest data, and index checkpoints have been
flushed and synced.

The durable append-log order is:

1. write commit-covered records, manifest, and index checkpoint as needed
2. flush and `sync_data` those bytes
3. write the commit marker
4. flush and `sync_all` the marker

### Single-barrier commit

`transaction_marker(on_flush, single_barrier)` /
`transaction_marker(explicit, single_barrier)` drop step 2: records and marker
are written and made durable by one `sync_all`. Storage may then persist the
marker without some bytes before it, so the marker (v2 payload `VCM2`) names
what it commits: the newest marker known durable when it was written (its
*anchor*, the last marker a completed `commit_durable` barrier covered), and
the count and order-sensitive checksum chain of every record after the
anchor's commit point. Every open route (scan, segment chain, header tails,
open digest) verifies the newest marker's run of records with full checksums
before treating it as a commit point; `crates/varlit/tests/single_barrier_commit.rs`
covers the scan, lazy-or-scan, header-tails and anchored routes. A run that does not verify is not
committed: the file ends at the anchor's commit point, a read-only open shows
exactly that prefix without changing the file, and a writer open truncates the
rest. Non-durable `commit()` markers after the anchor roll back with it.

The mode requires `integrity: crc32_with_header` (so every byte of every record
is under a checksum open can check) and refuses `recovery: mark_tail`. A writer
handle pays one extra `sync_all` before its first record, so that what an
earlier handle left unsynced is durable before it becomes the anchor. Strict
`open_*_lazy` routes return an error for an unverified newest transaction;
`*_lazy_or_scan` falls back to the scan.

Matrix `commit_*` calls stage commit bits without a sync. A reader sees them
only after a publication (see "What a publication is" above).

## Ordered Barrier Policy

The ordered barrier is opt-in per call rather than a format declaration, and is
intended for workflows that emit live progress only after durable commit.
`VarlitFile::write_matrix_cell_durable(key, value, hook)` runs the sequence with
file-handle `sync_data`/`sync_all`;
`write_matrix_cell_durable_with_barrier(key, value, &mut barrier, hook)`
substitutes a caller-supplied `MatrixDurabilityBarrier` for the two sync phases.
There is no `durability:` key in `varlit_format!` — the macro refuses one with
`unsupported varlit_format key` — and the hook is a closure argument rather than a
declared option.

The durable cell API runs:

1. stage the new slot (payload and CRC32) and clear the cell's commit bit,
   as `write_matrix_cell()` does;
2. call the data barrier;
3. stage the commit bit, as `commit_matrix_cell()` does;
4. call the commit barrier;
5. `sync()`: publish every staged matrix change as the next generation (see
   "What a publication is" above);
6. invoke the post-commit hook.

The barrier callbacks receive the primary file, which holds the matrix and its
ring. They must only synchronize it, not seek or write. Both callbacks precede
publication, so a fresh reader still sees the previous generation while they
run (`matrix_durable_barrier_observes_data_then_commit_sync_then_hook`). A
callback failure is returned as is, poisons the writer, and leaves readers on
the previous generation
(`barrier_failures_before_generation_publication_leave_readers_unchanged`).
Before step 5 the staged slot and commit bit are usually only in the writer's
memory (or in a tile appended since the last publication, or in uncommitted
ring frames), so the barriers alone do not make the new value durable; the
publication's sync does. Other staged matrix changes are published with the
cell.

## Hook Contract

The post-commit hook receives storage facts only:

```rust
pub struct MatrixCommitEvent {
    pub block_id: u32,
    pub category: &'static str,
    pub key: MatrixKey,
    pub slot_offset: u64,
    pub slot_len: u64,
}
```

Hook behavior is caller-owned. Varlit should not interpret progress messages,
network delivery, UI updates, or domain side effects.

If the hook fails after durable commit, the data remains committed. The error is
reported to the caller as a post-commit failure, not as a storage rollback.

That distinction is now in the type, not only in this document (F-04). A hook
failure returns
`Error::MatrixCommittedButHookFailed { event: Box<MatrixCommitEvent>, source }`,
the same shape as `PublishedButParentSyncPending` and
`ReplacePublicationIndeterminate`. Retrying the whole call after this variant
repeats the durable write and re-runs the hook, which is how a result-driven
retry used to duplicate external work. The carried event is the one the hook
was given; it is built from the cell's tile position before the commit, so
producing it cannot fail for a cell that is already durable.

If step 5 fails, the writer is poisoned and the hook does not run. When the
handle's matrix generation had not advanced at the failure, the result is
`MatrixPublicationUncertain`: the frames may or may not be on disk, and the
next writer open decides whether the generation counts. Reopen and inspect the
confirmed generation before retrying the write. When the generation had
advanced (the ring header was written and a later step of `sync()` failed,
such as the created pathname's parent-directory sync), the result is
`MatrixCommittedButDurabilityUnproven`. A successful publication followed by a
hook error returns `MatrixCommittedButHookFailed`; retry the notification
rather than the write.

## Allocator Failure And Published Outcomes

Published-outcome errors must preserve the distinction between a failed operation
and an operation that completed but needs recovery or notification. Allocation
refusal must not turn the latter into an apparently retryable append.

`write_matrix_cell_durable` reserves both boxes with `try_box`: the error box
before the working cell is written, the event box after that write and before
the cell is committed. The error box initially holds a scalar placeholder;
a later failure moves its actual source into that already allocated slot, so
both boxes keep their existing public types without a post-commit allocation.
The hook receives a clone of the event. A refusal of either reservation returns
before the cell is committed; a post-commit hook failure still returns
`MatrixCommittedButHookFailed` (both checked in
`crates/varlit/tests/fallible_publication.rs`). The successful durable-write
path pays these two allocations.

Record append, index publication and replacement errors use fallible boxing.
If detailed source/path storage cannot be allocated, they return
`Error::ErrorContextAllocationFailed` with an inline `AllocationFailureContext`:
append/commit sequence, published index, replacement rebind (including pending
parent sync), published replacement, or experimental reuse generation and its
uncertainty. Details can be lost; the completed/uncertain outcome is retained.
A caller must inspect that outcome before retrying. Stream sidecar wrapping preserves
a primary publication error even when an additional wrapper cannot be allocated.

This replaces the earlier abort-on-error-box policy. Preallocation of a scalar
placeholder makes it possible to reserve space before the eventual source error
exists; recursion in `Error` does not prevent allocating that box in advance.

These paths do not prove global allocation safety. Existing public `Clone` and
infallible convenience APIs remain compatible, `BTreeMap` allocation is deferred,
and formatting, application codecs/callbacks and other dependency internals need
separate coverage. An allocator that aborts internally, reference-count overflow,
an OS OOM kill or a power loss cannot be converted to an ordinary Rust error by
these helpers. Other allocation errors also retain their existing writer
poison/recovery rules; there is no blanket promise that retrying is safe.

## Sync Granularity

The first implementation may use file-handle `sync_data`/`sync_all` primitives
for portability. Later implementations can add platform-specific range syncs
without changing the logical barrier.

With the default `FileMatrixDurabilityBarrier`, `write_matrix_cell_durable`
calls `sync_data` on the primary file as the data barrier and `sync_all` as the
commit barrier, then publishes with the publication's own sync (two if the
write appended a tile), then invokes the hook.
`write_matrix_cell_durable_with_barrier` takes a caller-supplied
`MatrixDurabilityBarrier` for the two barrier phases, which lets tests and
policy adapters record or replace them; the publication's sync is not part of
the barrier. Sidecar-aware barriers remain future hardening work.

Per-record or per-cell fsync is not the default. Ordered barrier is chosen only
when callers need the stronger completion semantics.

## Sidecar Ordering

When a sidecar participates in the same logical commit:

1. write and sync sidecar data needed for resume
2. write and sync main file data/index
3. set and sync main commit bit
4. update sidecar progress metadata if configured
5. invoke post-commit hook

The current implementation provides an optional sidecar envelope and verified
resume signal. A separate durable parent sidecar-active flag remains future
policy work; callers that need that stronger contract should encode the active
state in their own payload or metadata until the policy is promoted.

## Matrix Sidecar Identity And Atomic Publication

The matrix sidecar manifest is version 3 (104-byte fixed header). Beyond the
existing format/schema/category/caller-generation/length/CRC fields it binds
the sidecar to a specific native file and a specific logical creation with
three additions:

- a native object fingerprint that folds the OS file identity (Windows
  volume + file id, Unix device + inode) with the schema hash,
- the offset of the matrix metadata record (still reported as
  `"matrix layout generation"` on a mismatch), and
- the 16-byte matrix creation nonce.

The creation nonce closes the same-object recreation gap: OS file identity and
the metadata record offset are stable when a matrix is recreated into the
same pathname/file object, so identity and offset alone could accept the
previous creation's sidecar. Every matrix create stores a fresh 128-bit nonce
in the matrix metadata record; the handle reads it when it opens its matrix and
keeps it. A caller-supplied application generation cannot substitute for the
native creation identity.

A reader recomputes the identity from its own native file and rejects a
mismatch as `MatrixSidecarMismatch("native identity")`,
`MatrixSidecarMismatch("matrix layout generation")`, or
`MatrixSidecarMismatch("creation nonce")`. A version-1, version-2, or
otherwise unrecognized envelope is refused as
`MatrixSidecarMismatch("sidecar version")`. Because sidecars are regenerable
resume state, a refused sidecar is a regenerate-and-retry signal, not data
loss — so neither a same-spec sibling file nor a recreated matrix in the same
file object can silently adopt another creation's sidecar. All fixed-header
identity fields and the small payload magic/category prefix are validated
before any payload allocation, read, or hash, so an obviously foreign sidecar
is rejected without configured-limit-sized work.

Publication is atomic and ordered. `write_matrix_sidecar` writes the new
manifest to a same-directory RAII temp file, `sync`s it, atomically replaces the
existing sidecar, and syncs the parent directory. The native file is the
authority: the caller publishes the matrix data the sidecar summarizes
(`sync()`) before writing the sidecar, since `write_matrix_sidecar` does not
sync the native file. A parent-sync failure after the replace returns
`PublishedButParentSyncPending` without deleting the now-published sidecar; the
temp is removed only when the replace itself failed. This replaces the former
in-place `File::create` truncate-and-`sync_all` publication, which exposed a
partially written sidecar during a crash.

## Acceptance Tests

Durability tests should use an injectable sync/hook recorder so ordering can be
verified without depending on real crash behavior:

- data sync occurs before commit sync
- commit sync occurs before post-commit hook
- hook failure does not clear a committed bit
- default policy does not perform per-cell sync

The injectable recorder verifies that a fresh reader still sees the previous
generation during both barrier callbacks, and sees the new generation after success.
- verified sidecar envelopes can be rejected before resume signal publication
