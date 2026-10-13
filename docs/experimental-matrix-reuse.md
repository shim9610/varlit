# Experimental matrix chunk reuse

Enable the Cargo feature `experimental-matrix-reuse`, then select
`experimental_matrix::MatrixOptions { reuse: true, ..Default::default() }` when
creating a file with `experimental_matrix::MatrixWriter`. Both switches are
opt-in. This is an **experimental API and file format**, not a replacement for
the ordinary matrix API. Its format may change before stabilization.

```rust,no_run
use stratava::{stratava_format, MatrixDimensions, MatrixKey};
use stratava::experimental_matrix::{MatrixOptions, MatrixWriter};

stratava_format! {
    pub format Samples {
        magic: b"REUSEEX1"; version: 1; endian: little;
        dims { row: u32, channel: u32, }
        commit: cell_bitmap { keyspace = [row, channel]; categories = [data]; };
        blocks {
            matrix Sample(id = 1, dims = [row, channel], category = data) {
                value: u64,
            }
        }
    }
}

let spec = Samples::spec().with_growing_matrix_dimension("row", 4096);
let dims = MatrixDimensions::from_pairs([("row", 4096), ("channel", 1)]);
let options = MatrixOptions { reuse: true, ..MatrixOptions::default() };
let mut writer = MatrixWriter::create(spec, "samples.reuse", dims, options)?;
let reader = writer.reader()?; // independent reader, private cache
writer.put(MatrixKey::new(0, 0), &Sample { value: 42 })?;
writer.sync()?; // publish this batch and make it durable
assert_eq!(reader.get::<Sample>(MatrixKey::new(0, 0))?.value, 42);
# Ok::<(), stratava::experimental_matrix::MatrixReuseError>(())
```

`reuse: false` delegates to ordinary native storage. Such readers retain their
snapshot until `follow(&mut self)`. The option is selected at **file creation**;
opening with the other setting fails. Toggling it is not an in-place conversion
and cannot restore historical snapshots after their slots have been reused.

## Reading and publication

With reuse enabled, readers automatically follow the latest confirmed generation.
Each successful `get` or `read_run_into` is from one generation. Separate calls
may see different generations. `read_run_into` traverses the declared contiguous
axis and may cross chunks. Missing cells return `MatrixReuseError::Missing`.

Each reader is `Send + !Sync`; open one per reading thread. Queries take `&self`.
`writer.reader()` shares only an `Arc<AtomicU64>` notification with that writer.
Independently opened readers, including other processes, validate the disk
publication heads before and after copying. There is no process-global reader
registry, cache mutex, shared-memory mapping, or cross-process atomic variable.
The atomic is a hint; disk heads remain authoritative, including if the writer
exits after publishing but before sending a notification.

A read that races publication discards its copied bytes and retries. Exhausting
`read_retries` returns `MatrixReuseError::Retry`; the caller may retry. A busy
writer can repeatedly outrun a reader. **The writer never waits for readers.**
The module exposes owned/typed reads, not mmap/borrowed views or pinned historical
snapshots. On any range-read error, discard its output (decoding can leave a
completed prefix). Reads copy and checksum chunks; this initial implementation
is not a zero-copy or sparse-page-update optimization.

`put` writes and marks a cell present; `delete` clears its presence bit. Both
remain private to the writer until `sync`. One sync publishes the whole batch,
including changes to multiple chunks/blocks. Explicit block Immediate requests
also invoke sync. Dropping a reuse writer **discards** an unsynced batch. There is
no implicit flush/publication on drop in this experimental mode.

## Space and I/O

Each logical chunk has two physical slots. The current confirmed slot remains
intact while the next value is staged; sync replaces the inactive slot and
publishes a new head. Repeated updates to an existing set of chunks stop growing
the primary after both slots have been allocated. The bound is approximately

```
12 KiB + 2 × (highest allocated chunk index + 1)
             × (64-byte slot header + chunk payload + presence bitmaps)
```

Sparse holes count toward logical file length but need not occupy disk blocks.
Growing into new chunks increases this bound. Deleting a chunk's cells does not
move its slots to another chunk or shrink the file. This initial allocator
reuses each chunk's own slots; it is not a general extent free list.

The `.reuse-work` and `.reuse-journal` companions hold pending chunk contents
and bounded-size journal entries (one per dirty chunk). They grow with the
current batch, then return to one 4 KiB header each after sync/recovery. Keep both
beside the primary. Renaming/moving a primary or opening it through a hard-link
alias is unsupported; the stored pathname binding is checked on open.

Reuse saves accumulated history space, **not the whole-chunk write cost**.
Dirty chunks are staged and then copied into reusable slots, so this prototype
can issue more writes than the ordinary append-only chunk implementation. It
also performs journal and publication durability barriers at sync. Choose a
chunk size in bytes, not only rows: 4,096 rows of 64-byte cells are 256 KiB at one
column and 160 MiB at 640 columns.

## Working-set controls and supported schemas

`MatrixOptions` defaults:

| Field | Default | Meaning |
|---|---:|---|
| `reuse` | false | Select experimental format explicitly |
| `rows_per_chunk` | 4,096 | First-axis entries per chunk in experimental storage |
| `max_chunk_bytes` | 16 MiB | Maximum one-chunk working buffer |
| `cache_bytes` | 2 MiB | Reader-local chunk retention; zero disables retention |
| `read_retries` | 8 | Bounded attempts per owned read (1–1,024) |

These bounds are independent of total file length. Open checks fixed metadata
and the declared geometry; it does not rebuild an all-chunk directory. Reader
scratch can hold one chunk even when retention is disabled. Materialization
limits also charge output, decode allocations and possible overlapping chunk
buffers. A chunk exceeding the working-buffer budget is refused; configure fewer
rows per chunk or a larger explicit budget. This prototype does not stream a
single oversized chunk through a smaller writer buffer.

Supported: uncompressed matrix-only schemas, a common first axis, a distinct
cell-bitmap category per block, fixed or growing first-axis capacity, and schema
checked typed cells. Experimental chunk rows come from `MatrixOptions`.
Unsupported native layout/record policies, auxiliary regions, shared/single/
per-channel commit categories and keyed blocks are rejected. Generic record,
metadata, mmap, opposite-axis range and migration APIs are not exposed by these
experimental handles. Use ordinary handles for those capabilities.

## Recovery and errors

The only writer admission is the OS file-object lock, released explicitly on
handle drop. Readers never acquire it. Sync first makes an undo journal durable,
then writes inactive slots and retirement metadata, makes data durable, and
publishes a checksum-protected head. A predecessor's retirement fence prevents
an erased newest header from silently reviving an older value. Readers can use
the durable undo record when retirement metadata is partially written.

Reopening a writer rolls back an unfinished publication or accepts an already
published one. Recovery streams journal entries with bounded memory. It restores
retirement metadata before clearing the durable journal. Thus an aborted slot
cannot become visible merely because a later transaction reaches its generation
number. A malformed recovery journal fails closed without attempting a guessed
repair; preserve the files for diagnosis.

A sync failure poisons that handle; drop it and reopen before writing again.
`PublicationIndeterminate` means the new generation may already be visible;
`Published` means it is durable but later cleanup failed. Inspect after reopen
before replaying application operations. Before-publication errors do not claim
success or durability. External mutation/truncation of managed files is not a
supported editing operation.
