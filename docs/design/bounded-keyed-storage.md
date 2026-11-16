# Bounded keyed storage

Status: design, agreed in discussion; not implemented. The indexed primary
(the first item under "What goes away") has been removed; the rest is pending.

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
| anything else (strings, sparse or random ids) | any | in-place records | section filters and a reader cache |

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

### Other keys: in-place records with section filters

Keyed records of this kind live inside internal region records
(`RECORD_FLAG_IN_PLACE`, as matrix tiles live inside overflow regions):
rewritten where they are, outside the sequence order and sync-marker chains
of the log, checked by their own CRC32s, changed atomically through the ring.
The log itself stays append-only: framed records are never written into a
hole, so the footer links of the log keep pointing backwards.

Finding them:

- **Section filters.** Each anchor section that holds keyed records carries a
  Bloom filter of their keys, a fixed number of bits per key (to be chosen;
  10 bits gives about 1 % false positives). Hashes are keyed with a random
  per-file seed so keys cannot be chosen to defeat the filters.
- **Filter hierarchy.** Every 32 sections have a filter of their keys, every
  32 of those another, so a lookup descends only through filters that may
  hold the key: four levels cover a million sections. Each level holds each
  key once, so the total is levels x bits per key, proportional to the data.
- **Reader cache.** Each reader has a fixed-size cache of recent key
  locations (recency and frequency, as ARC does) and of the top filter levels.
  A hit reads the record directly; a miss reads a few filters and one section.
- **Newest first.** Sections are searched from the newest, so the first match
  is the current value.

A lookup is always exact: a false positive costs one more section read, never
a wrong answer.

## Space reuse

Freed space is reused, not left behind.

- **Fixed blocks fill holes at once.** The encoded sizes of every fixed block
  are known at compile time. For each hole size the writer has a table,
  computed from them, of the combination of fixed blocks that fills it most
  tightly. A hole is filled by the next writes that combination admits; what
  is left is smaller than the smallest fixed block and stays on the free list
  of its region. Holes are inside region payloads, so no filler record is
  needed.
- **Only order-free records go into holes:** keyed in-place records, matrix
  tiles and directory pages, index and filter pages. Log blocks are always
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
filter, a run range, a matrix directory, the free list) is published, so a
crash before publication leaves the hole a hole.

## Readers

Readers keep their snapshot as matrix readers do: what the writer changes in
place goes through the ring, and a reader that falls behind the ring gets
`MatrixSnapshotExpired`-style expiry and follows. Space a published value
occupied is reused only after its generation is reclaimed.

## Implementation order

1. Separate the write-ahead ring and the region space from the matrix, so a
   keyed format owns them too.
2. In-place keyed records: update and delete in place, free list, ratio
   compaction, section filters and the reader cache, a per-key `get`.
3. Sorted runs for `key_order = increasing`.
4. Remove keyed history (`keyed_offset_chain`, `keyed_chain`, tombstone
   accumulation, `merge_keyed_files` and `compact_keyed_files`) in one change,
   with a one-time converter for existing files. Until then both paths exist.

## Open questions

1. Bits per key in section filters, and the filter fan-out, against the 1 to
   5 % budget. The first implementation uses 10 bits per key and a fan-out of
   32.
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
