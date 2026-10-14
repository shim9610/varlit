# Experimental matrix page reuse

Enable the Cargo feature `experimental-matrix-reuse`, then select
`experimental_matrix::MatrixOptions { reuse: true, ..Default::default() }` when
creating a file with `experimental_matrix::MatrixWriter`. Both switches are
opt-in. This is an **experimental API and file format**, not a replacement for
the ordinary matrix API. Its format may change before stabilization.
The current `STREUSE2` format uses pages and refuses older `STREUSE1` chunk files;
there is no automatic migration.

```rust,no_run
use varlit::{varlit_format, MatrixDimensions, MatrixKey};
use varlit::experimental_matrix::{MatrixOptions, MatrixWriter};

varlit_format! {
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
# Ok::<(), varlit::experimental_matrix::MatrixReuseError>(())
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
completed prefix). Reads copy and checksum the requested pages; this is not
a zero-copy API. Adjacent page pairs are read in bounded windows to avoid a
syscall per cell/page. Both physical slots are fetched in these windows, which
can increase sequential read traffic versus a single contiguous chunk.

`put` writes and marks a cell present; `delete` clears its presence bit. Both
remain private to the writer until `sync`. One sync publishes the whole batch,
including changes to multiple chunks/blocks. Explicit block Immediate requests
also invoke sync. Dropping a reuse writer **discards** an unsynced batch. There is
no implicit flush/publication on drop in this experimental mode.

## Space and I/O

Chunks remain logical row groups. Within each chunk, each block's presence
bitmap and cell data occupy separately page-aligned regions. Cells may span page
boundaries. Each logical page has two directly addressed physical slots; their
small checksummed headers identify which version belongs to the confirmed
generation. There is no growing pointer table or chain of diffs to replay.

Only modified pages are staged and published. Rewriting an already-present cell
does not dirty its bitmap; deleting it changes only the bitmap page. Byte-identical
writes do not dirty data pages. Sync replaces inactive slots and publishes a new
head. An unchanged page keeps its existing slot across generations.

Repeated updates to an existing set of pages stop growing the primary after
both slots have been allocated. The logical file-size bound is

```
12 KiB + 2 × (highest allocated page index + 1) × (64 + page_bytes)
```

Page alignment can leave holes, especially for small blocks. Sparse holes count
toward logical file length but need not occupy disk blocks. Growth into new pages
increases the bound. Deleting cells does not move their slots to other pages or
shrink the file; reuse is local to each page, not an arbitrary extent free list.

The `.reuse-work` and `.reuse-journal` companions hold pending pages and journal
entries (one 96-byte entry per dirty page). They grow with the current batch,
then return to one 4 KiB header each after sync/recovery. Keep both beside the
primary. Renaming/moving a primary or using a hard-link alias is unsupported;
the stored pathname binding is checked on open.

For a batch that stages each dirty page once, payload writes total twice the
modified page bytes: once to the work file and once to the primary. In addition,
each dirty page writes 288 bytes of headers/journal, and each nonempty sync writes
12 KiB of publication/intent/cleanup metadata and issues four durability barriers.
Thus a one-page overwrite with 4 KiB pages requests **20,768 bytes** of file writes,
including metadata. This is application I/O, not physical device write traffic.
A cell crossing two pages, a new presence bit, or changes in multiple blocks can
dirty more pages. Working-cache eviction and revisiting a dirty page can stage
it again before sync. Adjacent cached dirty pages are coalesced for staging;
primary slot/retirement writes remain per page.

Small overwrites benefit most. Full-matrix replacement still writes all changed
pages and adds per-page metadata/syscalls. Sequential reads may transfer both
slots and checksum many small pages. Page reuse is not a promise that every
workload is faster than the whole-chunk or ordinary native paths.

## Working-set controls and supported schemas

`MatrixOptions` defaults:

| Field | Default | Meaning |
|---|---:|---|
| `reuse` | false | Select experimental format explicitly |
| `rows_per_chunk` | 4,096 | First-axis entries per chunk in experimental storage |
| `page_bytes` | 4 KiB | Power of two from 4 KiB through 64 KiB; stored geometry |
| `max_chunk_bytes` | 16 MiB | Writer page-cache budget; legacy field name, not a logical chunk limit |
| `cache_bytes` | 2 MiB | Reader-local window budget; zero disables retention |
| `read_retries` | 8 | Bounded attempts per owned read (1–1,024) |

These bounds are independent of total file length or logical chunk size. Open
reads fixed metadata and geometry without rebuilding a page directory. Logical
chunks larger than memory budgets are supported: only relevant pages are loaded.
Writer pages use a private bounded cache; evicted dirty pages spill to the work
file. With a one-page cache, a separately charged 512-byte bitmap window
keeps presence checks from evicting dirty data on every cell. Only a changed
window is staged; returning to previously evicted windows can still repeat
bitmap-page writes. Larger caches retain the existing two-page hot path.
Cache capacity adapts to the materialization limit. The writer also needs
bounded stage/publication scratch and a cell encoding buffer; the cache budget
is not an exact total-RSS cap. A single oversized cell can still exceed limits.

Reader scratch holds at least one physical page pair even with retention off.
Bulk read windows and writer stage runs are capped at 256 KiB (or smaller
configured budgets). A window larger than `cache_bytes` is discarded after the
read. Materialization limits also charge result bytes and decode allocations.
`page_bytes` and `rows_per_chunk` must match on reopen; cache budgets and retry
limits can be changed independently for each handle.

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

## Targeted validation

The shared model in `crates/varlit/tests/support/matrix_reuse_model.rs` runs
both in ordinary regression tests and in two coverage-guided libFuzzer targets:

- `matrix_reuse_state_machine` compares pending and confirmed values against an
  independent in-memory model after writes, deletes, syncs, dirty drops and
  reopen operations. It checks both blocks, point reads and contiguous ranges.
- `matrix_reuse_publication` cuts publication at named I/O boundaries, reuses
  slots during a partially copied read, and corrupts current page checksums or
  undo entries. The oracle checks exact visible generations, rejects mixed or
  silently stale successful reads, and requires invalid undo entries to be
  rejected before recovery modifies the primary.

Inputs vary 4–64 KiB pages, logical chunk geometry, one-to-three-page writer
caches and disabled/small/window-sized reader caches. Cells span small pages;
the fixtures cross row and chunk boundaries. Each state-machine input runs at
most 64 operations on 12 cells in each of two blocks. These are small targeted
correctness cases, not throughput or endurance measurements.

Run the regression cases with the workspace runner:

```sh
cargo run -p varlit-test-runner -- test -p varlit \
  --features experimental-matrix-reuse,scalable-fault-injection \
  --test matrix_reuse_model
```

For coverage-guided mutation (nightly Rust and cargo-fuzz required):

```sh
cd fuzz
cargo +nightly run --locked --example generate_matrix_reuse_corpus
cargo +nightly fuzz run matrix_reuse_state_machine -- \
  -max_total_time=300 -max_len=196 -rss_limit_mb=4096
cargo +nightly fuzz run matrix_reuse_publication -- \
  -max_total_time=300 -max_len=196 -rss_limit_mb=4096
```

The sanitizer workflow generates these seeds and includes both targets in its
weekly/manual smoke run; failures retain reproductions and corpora as artifacts.
A passing finite run does not prove crash consistency on every filesystem or
replace process-kill, concurrent-process and real-platform qualification.
