# Bounded keyed storage

Status: in progress. The indexed primary is removed; in-place keyed storage
(step 2 below: record heap, slot buckets, compaction) is implemented as an
opt-in declaration; sorted runs and the removal of keyed history are pending.

## Problem

Updating a keyed record appends a new version and leaves the old one in the
file. The file therefore grows with the number of updates, not with the
information it holds, and nothing bounds that growth:

- an indexed primary (`key_index = disk`) has no way to reclaim superseded
  versions; `compact_index()` compacts only the key index;
- a native file grows the same way through `push_keyed`, `delete` tombstones
  and `replace_block`, and reclaims space only when the application calls
  `defragment()`, which rewrites the whole file and needs room for two copies.

Repeatedly updating one key is enough to fill a disk while the live data stays
small. Where untrusted input can cause updates this is a denial of service
(uncontrolled resource consumption). Nobody expects editing one cell of a
spreadsheet to grow its file without limit, and varlit should not either.

## Principles

1. **Disk follows information.** A file's size is its live data, plus a
   bounded overhead: free space of at most 1 to 5 % of live data (a declared
   ratio), summaries proportional to bytes written, and fixed structures (the
   write-ahead ring). Updating or deleting never grows a file beyond that.
2. **Memory is fixed at open.** A handle allocates its buffers when it is
   opened, sized by the format and declared limits; data paths allocate
   nothing after that (see `AGENTS.md`).
3. **No size refusals.** Work larger than a buffer or a fixed structure
   streams through it, as matrix publications do.
4. **Append order is kept where it is promised.** Unkeyed log blocks stay
   append-only: file order is write order, and their growth is information.

## What goes away

- **History of keyed records.** `keyed_chain`, `prev_same_key_offset` and the
  `keyed_offset_chain` index policy. A key has one current value.
- **The keyed tail map** of native writers, which existed to maintain that
  chain and costs memory per key.
- **Accumulated tombstones.** A delete frees the record's space.
- **The indexed primary** (`VarlitIndexedWriter`, `VarlitIndexedReader`, the
  `.vki` sidecar and `key_index = disk`), whose job (key lookup without
  memory per key, constant-cost open) the structures below take over inside
  the primary file. What happens to the stream handles (`.vks`) is an open
  question below.
- **Dependence on `defragment()`** for keyed data. It may stay as an explicit
  whole-file rewrite, but bounded size never needs it.

## Where keyed data lives

The format declaration decides, at compile time, which of three placements a
keyed block uses. A keyed block is placed in place by default; a block
declared `key_order = increasing` uses sorted runs. An application with
integer keys and fixed-size values chooses a matrix itself, as it does today.

| Keys | Values | Placement | Lookup |
| --- | --- | --- | --- |
| integers, dense enough to tile | fixed size | a matrix | arithmetic: key to tile and slot |
| increasing (time, sequence, position id) | any | sorted runs in the log | binary search over anchor key ranges |
| anything else (strings, sparse or random ids) | any | record heap | slot buckets by seeded hash |

### Integer keys and fixed-size values: matrices

A matrix already meets every principle: cells are overwritten in place through
the ring, publications of any size stream through an on-disk index, and a
handle allocates nothing after open. An application with integer keys maps
them to matrix coordinates. Very sparse keys over a huge range waste up to a
tile per key, so such formats declare small tiles.

### Increasing keys: runs sorted at sync

The writer keeps keyed records staged since the last sync in a fixed buffer
and writes them sorted by key at sync (a full buffer is sorted and written
early). Each sync's records form a run in key order. The anchor after a run
records its first and last key (a few bytes). A lookup binary-searches the
anchors for the runs whose range holds the key, newest first, and reads one
section of the run. With increasing keys the runs do not overlap, so a lookup
costs about `log2(sections)` reads and a range query reads only the sections
it covers.

An update of a key already written is an in-place update (below), so runs
never hold two versions of a key.

### Other keys: a record heap and slot buckets

Keyed records of this kind live in a record heap: internal region records
(`RECORD_FLAG_IN_PLACE`, as matrix tiles live inside overflow regions) filled
densely with records, each checked by its own CRC32 and changed atomically
through the ring. The log itself stays append-only: framed records are never
written into a hole, so the footer links of the log keep pointing backwards.

Finding them:

- **Slot buckets.** A key's seeded hash picks a bucket, a 4 KiB page of
  slots. A slot is the key's fingerprint and its record's offset (12 bytes).
  Buckets grow by linear hashing: when one is full, the bucket at the split
  pointer is split in two, so the table grows one bucket at a time and never
  rehashes as a whole. The hash is keyed with a random per-file seed so keys
  cannot be chosen to fill one bucket.
- **Bucket directory.** Bucket numbers map to bucket pages through a small
  radix tree of pages, as matrix tiles map to their pages.
- **Lookup.** Hash the key, read the bucket page (the upper directory levels
  are in a fixed reader cache), read the record whose slot fingerprint
  matches, and compare its key. Usually two reads; never a wrong answer.
- **Update.** A value that fits where the old one is is overwritten there.
  Otherwise it goes into a hole or at the end of the heap, the slot's offset
  changes, and the old place becomes a hole.
- **Delete.** The slot is removed and the record's place becomes a hole. No
  tombstone stays.

Buckets are not full on average (a split leaves two half-full buckets), but
that slack is in 12-byte slots, not in records: about 16 bytes per key in
all. This index cost is reported separately and is not part of the
free-space ratio, which governs holes in the record heap.

## Space reuse

Freed space is reused, not left behind.

- **Fixed blocks fill holes at once.** The encoded sizes of every fixed block
  are known at compile time. For each hole size the writer has a table,
  computed from them, of the combination of fixed blocks that fills it most
  tightly. A hole is filled by the next writes that combination admits; what
  is left is smaller than the smallest fixed block and stays on the free list
  of its region. Holes are inside region payloads, so no filler record is
  needed.
- **Only order-free records go into holes:** keyed heap records, matrix
  tiles and directory pages, bucket and index pages. Log blocks are always
  appended.
- **Variable-length records** go into a hole that fits when there is one; the
  remainder of the hole returns to the free list.
- **The free list** is held in a fixed-size table in memory. When it fills,
  entries spill to free-list pages in the file, listed from the metadata the
  way overflow regions are, and changed through the ring so a crash leaves a
  consistent list. Free-list pages are themselves reusable space.
- **The ratio is the backstop.** When free space exceeds the declared ratio
  (1 to 5 % of live data, declared in `varlit_format!`, 5 % when omitted, and
  recorded in the file header) because nothing has filled it, the writer
  compacts once: it moves live records out of the sparsest regions and frees
  them whole.

Writing into a hole is safe across a crash for the same reason carving matrix
tiles is: a record is written and synced before anything that locates it (a
slot, a run range, a matrix directory, the free list) is published, so a
crash before publication leaves the hole a hole.

## Readers

Readers keep their snapshot as matrix readers do: what the writer changes in
place goes through the ring, and a reader that falls behind the ring gets
`MatrixSnapshotExpired`-style expiry and follows. Space a published value
occupied is reused only after its generation is reclaimed.

## Implementation order

1. Separate the write-ahead ring and the region space from the matrix, so a
   keyed format owns them too.
2. Keyed records in the record heap: update and delete (new values to new
   space, see below), slot buckets, free list, ratio compaction, a per-key
   `get`.
3. Sorted runs for `key_order = increasing`.
4. Remove keyed history (`keyed_offset_chain`, `keyed_chain`, tombstone
   accumulation, `merge_keyed_files` and `compact_keyed_files`) in one change,
   with a one-time converter for existing files. Until then both paths exist.

## Placement within the heap

The first implementation overwrote a value in place when the new one fit,
through the write-ahead ring. Benchmarks (`tools/format-bench`, oltp and grid)
showed the cost: every update was written twice, once into the ring and once
into place, and a 16 MiB ring filled every few dozen transactions, so one in
about 37 stalled for a 27 ms checkpoint. Values now go to new space instead
(copy on write within the heap): space freed by a generation already in
place, written straight to the file; only slots, bucket pages (staged whole,
once per publication, from 64 page buffers) and region headers (also once per
publication) go through the ring. The file still keeps one copy of each key
plus the values the latest publications replaced, which updates take after
the next checkpoint. Measured on the same oltp run: p99 27 ms to 4.2 ms,
p50 0.9 ms to 1.5 ms (a second barrier per publication), file size unchanged.

## Open questions

1. Settled: slot buckets instead of section filters (filters need a newest-
   first search over many versions, and in-place records keep one).
2. Settled: the free-space ratio is declared per format, 5 % by default.
3. Whether the stream handles (`.vks`, constant-cost open of unkeyed logs)
   stay: anchors may already give what they give. They stay for now: stream
   `sync` and `follow` run through the verified publication drivers.
4. Whether the native record directory (13 bytes per record in memory) can be
   replaced by anchor-based lookups, which the memory principle asks for.
5. Settled: no filler record; holes stay inside region payloads.
6. Migration: files with keyed history or an indexed primary need a one-time
   conversion (read with the old version, write with the new), as 0.12 matrix
   files do.
