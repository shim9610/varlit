# Bounded keyed storage

Status: in progress. The indexed primary is removed; keyed storage (step 2
below: record heap, slot buckets, compaction) is implemented as an opt-in
declaration and is being reworked to hold the bounds under "Bounds at the
extremes"; sorted runs and the removal of keyed history are pending.

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

## Bounds at the extremes

The first heap failed at scale where small tests could not reach: one
publication rewrote 65 536 chunks, the 4096-entry hole cache forgot most of
the freed space, and every step grew the file by the data's size. A fixed
structure must never bound what the writer can reuse, and no publication may
need extra space in proportion to its size. The rules below hold for any
sequence of operations; the writer checks them in tests and the space policy
is verified as a state machine.

**Change budget `B`.** A format declares how many bytes one round may
allocate (`KeyedStorage::change_budget`, 64 MiB by default, recorded in the
metadata). Puts accumulate in a round; when the next allocation would pass
`B`, the writer publishes the round and checkpoints it, so the space the
round freed is reused by the next one. A `sync` that changes more than `B`
is therefore several publications, each atomic; a crash keeps a prefix of
rounds. Extra space for values not yet reusable never exceeds about `2B`.

**Records larger than `B`.** Replacing a value atomically needs the old and
the new bytes at once, so a record larger than `B` follows the format's
`LargeRecords` policy:

- `Stream` (default): a begin signature marks the record as being replaced
  (one round; readers then find no record for the key, `get` returns
  `false`), after that round is in place the record's extents are
  overwritten and extended in rounds of at most `B`, each publishing how many
  bytes are durable, and a completion signature makes the new value visible.
  There is no rollback: a crash leaves the record incomplete. Puts of the
  same value resume it (the durable prefix is checked against its checksum),
  and incomplete records can be counted, listed and discarded.
- `Refuse`: a record larger than `B` fails with `LimitExceeded`.

**Free space only at the tails (size classes).** Searching for a hole that
fits cannot be complete with fixed memory, so the heap never has holes to
search for. Items are kept in size classes, spaced 1/16 apart from 32 bytes
to 64 KiB (16-byte steps below 256 bytes): about 130 classes. Each class is a
dense array of fixed-size items with all its free space at its tail; a new
item is appended at the tail, and an item freed in the middle is filled by
moving the class's last item into it and repointing whoever pointed at the
moved item. No search, no fragmentation, no compaction.

- A record is a chain of items: full 64 KiB items, then one item of the
  class its remainder fits. Rounding to a class wastes at most 1/16 of a
  record (and 16-byte alignment below 256 bytes).
- An item header (32 bytes) holds its payload length, the record's total
  length (head item), the next item of the chain, the previous one (or, for
  a head, the key's route) so a moved item's referrer can be found, flags
  (streamed replacement in progress) and a checksum.
- A slot points at a head item by class and index. A class maps indexes to
  pages through a radix directory like the bucket directory. Pages are
  64 KiB records, all the same size: a page left empty at a class's tail
  goes on a free page list shared by every class.
- **Atomicity.** An item freed in a round may still be read through a
  published generation (by a reader, or by a recovery after a crash), so it
  is written again only once no generation that uses it can be shown. A
  freed item gets the generation that freed it in its header (`dead`) and
  is logged (16 bytes, appended to log pages). Once the ring's mark reaches
  that generation, readers older than it expire and the last publication no
  longer uses the item: the class's last live item is written into it once
  (its bytes straight to the file, its header through the ring so a crash
  leaves the hole marked dead) and its referrer repointed, and the class
  shrinks by one; a dead item at the end just shrinks the class. Every item
  below a class's end is live or dead and logged, and nothing else, so
  there is nothing to search for and nothing to track besides the log.
- **Taking a hole directly.** When the oldest logged item is of the class a
  put needs and the mark has passed it, the put writes its item there (header
  through the ring) instead of appending and moving later.
- **The mark, not a checkpoint.** The writer raises the mark to its last
  publication (one header write) when freed items have waited past the
  free-space ratio, or at the end of a round. Older readers must follow.
  Nothing has to be put in place for freed space to be reused.
- **Where bytes go straight to the file.** An index below the largest end
  any generation from the mark on had may still be read: it changes
  through the ring. Past it, and in dead items the mark has passed, bytes
  are written in place when nothing staged covers them.
- **Units.** A reader's overlay keeps one range per start offset, so every
  structure is staged in the same pieces every time. A class page's first
  16 bytes belong to the pool link and never to an item, and a pooled page
  is taken again only after the last generation that staged its link is in
  place: pages pass between classes whose items start at other offsets.
- History: an earlier version kept up to the ratio of holes inside the
  classes, tracked by per-class bitmaps, so that holes could be reused
  without moving items. The bitmaps never shrank below the most a class
  ever held, which broke the bound over the data held now; filling every
  hole from the end once the mark passes it costs the same single write per
  move and needs no bitmaps.

**Shrinking.** Deletes merge buckets (linear hashing in reverse, with a gap
between the split and merge thresholds so alternating inserts and deletes do
not thrash); class tails shrink page by page onto the free page list.
Returning space at the end of the file to the filesystem needs the native log
to cut trailing records safely (commit markers, anchors and digests); it is
designed separately.

**The bound.** Apart from the end-of-file question above, at every point the
pages in use (all but pooled ones) hold the live items, the items the log
names, a partial and a spare page per class (about 107 × 2 × 64 KiB) and the
log's pages, and nothing else (`idle_bound`, checked after every operation of
the tests). The log names at most twice the patience plus three rounds (a
round being the change budget and one record) (`logged_bound`): the open
round's frees, the published ones waiting for the mark (the mark is raised
once they pass the patience), and those the mark has passed, which never
outgrow the largest batch that waited for it because each operation takes
at least as many logged bytes as it frees (a put that writes into a logged
item counts). Both are bounds over the data held now, not the
most ever held. Rounding adds at most 1/16 of live. Pooled pages are reused
before the file grows; returning them to the filesystem is the end-of-file
work. The ring and the bucket and directory pages the live keys need come on
top.

## The B-tree of 4 KiB pages (version 5)

This replaces both earlier tree attempts below (64 KiB pages with values in
size-class chains, then leaves of key prefixes). Measurements showed what
mattered: lookups read whole 64 KiB pages per level, and every value lived
outside its leaf, so a lookup and an update each touched far more bytes
than the record. Keyed storage is now laid out the way SQLite lays out a
table, inside varlit blocks.

- **Pages.** 4 KiB, carved from blocks the writer appends as it needs them
  (16 pages, then twice the last, up to 1 MiB). Freed pages go on a free
  list and are taken again once the ring's mark reaches the generation that
  freed them.
- **Leaves hold the records.** A record of up to 1024 bytes is held whole
  in its cell; a longer one keeps its first bytes in the cell (sized so the
  rest fills whole overflow pages) and the rest in a chain of overflow
  pages. A lookup reads one page per level, and the leaf usually has the
  value.
- **Interior pages hold separators**: the shortest prefix between two
  neighbouring keys, at most 256 bytes in the page, the rest in overflow
  pages. A tree of 4 KiB pages holds a few hundred entries per interior
  page: four levels reach billions of keys.
- **Changes go through the ring**: a changed page is staged whole, so a
  publication is atomic. Overflow pages no generation uses yet are written
  in place.
- **The ring is small** (2 MiB by default, a fixed part of the file): a
  round ends before its publication would outgrow half the ring and stream
  into an index appended to the file, so bulk loads publish more often
  instead of growing the file.
- **Space.** Pages are kept at least half full (splits halve a page;
  merges join neighbours that fit one page), and freed pages are reused
  before the file grows. The strict free-space ratio of the earlier designs
  is given up: it cost about `1/ratio` bytes of repacking per byte freed.
  Deleting most keys leaves their pages in the free list for new keys; a
  file that shrank for good is rebuilt, which also returns the space to the
  file system.

## Superseded: sorted pages with key prefixes (version 4)

The slot buckets above keep one 16-byte slot per key: the index grows with
the number of keys, and the pages a table gives up when keys are deleted
stay behind as bucket pages no other structure can use. Version 4 replaces
them. The index holds one boundary per page of records, never one entry per
key, and every page it uses comes from and returns to the shared page pool.

**Declaration.** `KeyedStorage::with_key_index(KeyIndex::Sparse | KeyIndex::Off)`
(DSL: `key_index = sparse | off`):

- `Sparse` (default): records sorted by key in pages, found by key.
- `Off`: no key index. A keyed block is addressed by its position in the
  block (`put_at` / `get_at` / `remove_at`), and the position is the sort
  key: the same tree, with an 8-byte big-endian ordinal in place of the key.
  Nothing derived from user keys is stored.

**Leaves.** A leaf is a 64 KiB pool page holding one entry per record, in
key order:

- after the page link (16 bytes): a header (`magic b"VKLF" | count u16 |
  high u16 | garbage u16 | zero`), then `count` 4-byte entries
  (`offset u16 | length u16`) sorted by key, growing up; bodies grow down
  from the end of the page. Free space is the gap between them, plus the
  bodies of removed entries until the page is repacked.
- an entry's body: `flags u8 | key length (varint) | the key's first 32
  bytes | total length u64 | head item u64`. The record itself, its key
  then its payload, is a size-class item chain, as before.
- Values never live in leaves. A first version kept small values inline;
  then a value whose size changed changed its leaf, and keeping leaves
  within the ratio cost about `1/ratio` bytes of repacking per byte changed
  (measured: about 50 times the bytes written, 5 ms per update). With the
  value in a chain, an update writes the new chain and rewrites the entry
  in place at the same length: the leaves do not change size, and only
  inserts and deletes of keys pay for packing, on entries of a few dozen
  bytes.
- Keys longer than 32 bytes are compared by reading the rest from the
  chain. The key is stored once in the chain and its first bytes again in
  the entry, at most 32 bytes per record.

**Interior pages.** The same page format with entries `child page u64 |
separator`, where the separator is the shortest byte string `S` with
`left maximum < S <= right minimum` (bytes compared lexicographically, the
block id first). A separator longer than 256 bytes is stored in an item
chain owned by the entry. The root and the height are in the state.

**Lookups.** From the root, each page is searched by binary search over its
entries, reading only the entry array and the keys the search probes (a
handful of small reads per page, through the reader's snapshot). A page of
fan-out about 1000 keeps a terabyte of records three levels deep; the index
is the interior pages, about 0.1 % of the data, and none of it is per key.

**Writes.** A tree page is changed through the ring in whole 4 KiB segments
(the 64 page buffers the writer already has hold the segments being edited;
a dirty segment is staged when evicted or at publication). A new page from
the pool is written in place when no generation from the mark on uses it.
A leaf that overflows is split by bytes (at the end for appends in key
order) and its separator inserted in the parent; a parent that overflows
splits the same way; a root split adds a level.

**The space bound.** Free bytes inside leaves are counted in the state.
While they exceed the declared ratio of live bytes (at least two pages),
every operation also packs: a cursor walks the leaves in key order and
moves records from the leaf after it into the free space of the leaf at it,
removing a leaf that empties (its page to the pool, its separator from the
parent, interior pages merging below a quarter full). Each operation packs
at least as many bytes as it freed or left free, so the free bytes inside
leaves stay within the ratio plus a constant, over the data held now. The
cost is the price of the bound: with a ratio `r`, a freed byte costs about
`1/r` bytes of repacking in the worst case; since values live in chains,
only inserts and deletes of keys pay it, on entries of a few dozen bytes.
Interior pages, about 0.1 % of the data, are removed when they empty and
are not packed.

**What stays.** The ring, the mark, the freed-item log and the size classes
(every record's key and value), the page pool and its link
area, the change budget and rounds, readers' snapshots and expiry.

**What goes.** Slot buckets, the bucket directory and its free list, linear
hashing, the SipHash route (ordering replaces hashing: keys are compared,
not hashed).

**Proofs.** Kani covers the page and entry layouts, the binary search over
an entry array of a bounded page, the separator choice (`left maximum < S <=
right minimum` for keys up to a bound), and the varint. Splits, packing,
crash atomicity and the space bound are covered by native tests that check
every structure and the bound after every operation, and by the model fuzz
target.

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
