> Measurement appendix to the [canonical combined report](../workload-storage-20261104/README.md) and [integrated dashboard](../workload-storage-20261104/index.html). These results are integrated there; this page retains the detailed cohort methods and controls.

# Grouped logical items: Varlit / SQLite / HDF5

The grouped layout with packed items and ordered block keys reduces Varlit's individually
framed storage/time cost. This is a layout comparison on the same production
runtime, not a production-code optimization.

The prior million-item fixture already batched 1,000 records per publication,
but retained one physical Frame and general-index key per item. Its result
must not be used as representative grouped-item performance. We retain those
observations and add equal-publication individually framed controls here.

[All results](all-results.md) · [Metrics CSV](metrics.csv) · [JSON](comparison.json)
· [Harness/contracts](../../../tools/storage-bench/README.md)
· [Grouping manual](../../scalable-io.md#choose-the-physical-block-before-choosing-the-batch-size)

## Logical items, physical records and publication

Each logical item has an immutable dense u64 ID and 128 caller-visible bytes,
including that ID. Routing is declared: divide the ID by items-per-block to
find its block, then take the remainder for its slot. A physical Frame contains
512 or 8,192 items. Its encoded block key is indexed in Varlit/SQLite; HDF5
persists a dense group-to-payload-extent directory. This is not a replacement
for a general arbitrary-key routing index, nor a million schema/block types.

| Logical payload | Logical items | Items/block | Physical blocks | Publications |
|---|---:|---:|---:|---:|
| 128 MiB | 1,048,576 | 512 | 2,048 | 8 |
| 128 MiB | 1,048,576 | 8,192 | 128 | 8 |
| 1 GiB | 8,388,608 | 512 | 16,384 | 64 |
| 1 GiB | 8,388,608 | 8,192 | 1,024 | 64 |

Primary publications contain 131,072 logical items / 16 MiB, independently of block
size. Mixed Metadata and application Head records remain included. Logical
values, publication sizes and required operations match across engines.
Changing the number of physical records is the experiment, not missing work.
General block keys remain unrestricted to support growing capture logs; the
finite-key enum policy is not used as an implicit cap on future block count.

The 42 grouped configurations cover both sizes and block widths, one/four
producers and eight readers, plus 128 MiB one-reader controls. Six 1 GiB
four-producer/eight-reader configurations additionally compare follow-only
stream/BLOB/HDF5 layouts. Five 128 MiB individually framed controls use the
same 131,072-item publication boundary, one producer and eight readers.
Six further keyed profiles hold the 1 GiB block layout fixed and compare
1 MiB publications with the primary 16 MiB publications.

## Required read work and concurrency

Each reader consumes the full published workload, validates every item byte,
missing/duplicate block ID, metadata and writer-derived order. Keyed profiles
then perform a **separate backend block lookup**, and copy/check every logical
item in reverse slot order through a caller-owned 128-byte buffer. The fetched
block is retained only for that item-query batch. The scan payload is not used
as the point result. This is group-local batched item lookup, not unrelated
cold random queries or one storage syscall per logical item.

Varlit uses indexed block Frames, SQLite uses a keyed BLOB table and native
prepared statements/WAL/FULL, and HDF5 uses sealed payload chunks, persisted
group directory and an application SWMR publication prefix. Follow-only layouts
omit unneeded point indexes. These integrity/recovery contracts differ; no
SQL ACID equivalence is asserted. No live backing file is memory-mapped.

Varlit/HDF5 have one writer receiving transactions through MPSC capacity two.
One/four producers retain their own staging in addition to the queued and active
transactions. SQLite producers own independent FULLMUTEX connections; SQLite
still permits only one active write transaction per database. Every reader has
an independent process/handle/cache. Writers do not wait for reader acknowledgments.

## Before/after layout controls

The following all process 1,048,576 logical items / 128 MiB with one producer,
eight readers and eight publications. Keyed cases include all group-local item
queries; the individual control executes a backend point query per item.

| Layout | Full write s | Reader-active s | Complete s | CPU s | Final MiB | Aggregate PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| varlit-indexed: individual Frames | 25.656 | 26.049 | 27.818 | 107.943 | 363.770 | 219.97 |
| varlit-indexed: 512 items/block | 0.264 | 0.466 | 0.551 | 2.011 | 130.000 | 92.85 |
| varlit-indexed: 8192 items/block | 0.279 | 0.670 | 0.753 | 2.681 | 128.242 | 122.95 |
| sqlite: individual Frames | 4.331 | 4.162 | 4.822 | 17.809 | 180.406 | 190.28 |
| sqlite: 512 items/block | 0.553 | 0.525 | 0.720 | 2.285 | 146.754 | 158.23 |
| sqlite: 8192 items/block | 0.470 | 0.495 | 0.671 | 2.104 | 129.629 | 172.52 |

Grouping changes the number of physical lookups by making the application
block route and locality explicit. It does not demonstrate the same speedup
for arbitrary scattered keys. The individual Frame control retains its earlier scattered composite-key encoding;
grouped block keys retain natural group-ID order. The layout comparison therefore
changes framing, index granularity and key locality together. Publication size
is fixed here; the separate publication contrast holds those layout choices fixed.
The individual SQL control uses recorded default
page/mmap settings; grouped SQL uses its separate tuning winner. Compare the
Varlit layout controls as layout changes, not a claim of globally optimal SQL.

## Large concurrent workload

Each of eight readers processes the entire 1 GiB workload; there are four
producers feeding one active writer. All rows below include grouped item queries.

| Layout | Full write s | Reader-active s | Complete s | CPU s | Final MiB | Aggregate PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| varlit-indexed, 512/block | 2.567 | 3.970 | 4.072 | 15.375 | 1101.082 | 169.11 |
| sqlite, 512/block | 4.744 | 4.082 | 4.837 | 16.926 | 1170.941 | 171.15 |
| hdf5, 512/block | 2.860 | 5.803 | 5.933 | 23.273 | 1025.547 | 236.92 |
| varlit-indexed, 8192/block | 1.958 | 5.032 | 5.166 | 19.556 | 1026.180 | 192.09 |
| sqlite, 8192/block | 4.511 | 4.012 | 4.605 | 16.339 | 1033.629 | 197.91 |
| hdf5, 8192/block | 2.775 | 5.999 | 6.123 | 23.924 | 1024.141 | 246.72 |

Full write includes initialization, ingest, maintenance and final close/checkpoint.
Reader-active is the maximum reader's consume+refresh work, excluding idle
sleep but including tail work after the writer stops. Complete includes reader
startup/drain. These counters overlap and must not be added. CPU is kernel
wait4 user+system including waited-for child readers.

## Reads completed while writing

Both endpoints of a consume interval must fall inside the common-clock
writer-active window to count as entirely concurrent. Intervals crossing the
window edge are excluded, not proportionally assigned. This is a strict lower
bound: an excluded crossing interval may have performed much of its work while
the writer was active. During-write % below
uses the least-advanced reader; its MiB/s divides those bytes by the full
writer window, including waits. Each consume interval includes its item-query
batch. Throughput normalizes progress by one logical payload pass, not all
physical reads or the extra point-result copies. This separates observed
concurrent progress from final drain.

Publication occurs inside a backend write call. The p99 delay to completion of the benchmark consume helper
uses call return/start as lower/upper bounds, respectively. SQLite
lock waits can widen the upper bound. It is not an exact commit timestamp,
first-byte latency, an individual item API latency or a fixed-rate real-time guarantee.

| 1 GiB, four producers/eight readers | During-write % | Minimum reader MiB/s | Final drain s | p99 delay bounds ms |
|---|---:|---:|---:|---:|
| varlit-indexed, 512/block | 21.88 | 87.43 | 1.4701 | 2624.63–2657.52 |
| sqlite, 512/block | 90.62 | 228.58 | 0.1986 | 503.61–2789.30 |
| hdf5, 512/block | 18.75 | 53.89 | 3.0253 | 4377.67–4418.10 |
| varlit-indexed, 8192/block | 6.25 | 37.40 | 3.1779 | 4207.47–4236.73 |
| sqlite, 8192/block | 89.06 | 234.41 | 0.1933 | 872.23–2172.78 |
| hdf5, 8192/block | 21.88 | 80.89 | 3.3138 | 4380.43–4414.56 |

## Publication size with the physical layout held fixed

The same 1 GiB, four-producer/eight-reader group layout is measured with
1 MiB and 16 MiB publications. This isolates publication amortization from
block packing and key order. Larger publications also retain more producer
staging and delay visibility; memory and completion remain charged.

| Engine | Items/block | Publication MiB | Full write s | Complete s | CPU s | Aggregate PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| varlit-indexed | 512 | 1 | 7.221 | 7.268 | 22.822 | 36.31 |
| varlit-indexed | 512 | 16 | 2.567 | 4.072 | 15.375 | 169.11 |
| sqlite | 512 | 1 | 5.224 | 5.299 | 18.092 | 109.88 |
| sqlite | 512 | 16 | 4.744 | 4.837 | 16.926 | 171.15 |
| hdf5 | 512 | 1 | 6.388 | 6.733 | 25.958 | 110.34 |
| hdf5 | 512 | 16 | 2.860 | 5.933 | 23.273 | 236.92 |
| varlit-indexed | 8192 | 1 | 7.347 | 7.413 | 24.953 | 67.41 |
| varlit-indexed | 8192 | 16 | 1.958 | 5.166 | 19.556 | 192.09 |
| sqlite | 8192 | 1 | 5.167 | 5.322 | 18.828 | 117.30 |
| sqlite | 8192 | 16 | 4.511 | 4.605 | 16.339 | 197.91 |
| hdf5 | 8192 | 1 | 7.156 | 7.375 | 28.557 | 114.89 |
| hdf5 | 8192 | 16 | 2.775 | 6.123 | 23.924 | 246.72 |

## Resource accounting and qualification

The existing 4 CPU / 16 GiB / 32 GiB limits were not raised. Aggregate PSS/RSS
are sampled across writer, wrapper and all readers; producer threads are part
of writer memory. A complete process-tree coverage check is required. Samples
can miss shorter memory peaks. Unmapped OS file cache is not process PSS;
cgroup file/anonymous memory and pre-run baselines are retained, but include
the rest of this shared environment.

Final MiB is filesystem-allocated storage across primary and companions after
readers and writer close. Per-file logical/allocated sizes are retained in JSON.
Peak visible storage, temporary/WAL bytes and deleted-open generations are in
metrics.csv and raw probes. Temporary bytes are a subset of visible bytes;
independent maxima must not be summed as a simultaneous peak.

There are **141 selected successful timing observations and 47 qualified separate
resource probes**, three timings and one probe per configuration. Min/max values
are retained. Three repetitions are observational medians, not a 5% performance
qualification. SQLite tuning is 12 complete configurations × three observations
at 128 MiB/one producer/eight readers; each block width's complete winner is
frozen before the main run. Transfer to other sizes/concurrency is not a global
optimality claim. All queried values are immutable and known published IDs.

Two SQLite 1 GiB timing attempts were stopped by the existing free-space guard.
Their data/log/hash evidence is retained; after reclaiming only rebuildable old
build caches, both were retried successfully at the same limits and frozen
binary identity. This is not a larger storage budget. A periodic resource
sample also fell short of a child's final file allocation; its incomplete
observation is retained. A final quiescent file observation was added and all
36 earlier grouped resource profiles repeated with the same frozen binaries. Two
earlier successful resource samples are superseded, not pooled. One individual
control overlapped CI helper tests; it is retained/excluded and replaced by a
serial control observation. These earlier cohorts use scattered group keys and 1 MiB publications. They are retained as superseded evidence and are not pooled into final v2 results. The final grouped-v2/control-v3 selection uses naturally ordered group keys and includes a controlled 1 MiB versus 16 MiB publication comparison. The initial 16 MiB individual HDF5 control used 4 MiB header chunks exceeding its roughly 1.3 MiB dataset cache and was stopped after preserving the completed 66.886-second observation and partial attempts. Header chunks were capped at a transaction-dividing 4,096 rows (128 KiB for Frame headers), without increasing cache budgets; the complete controls were rerun. Grouped profiles have at most 256 physical Frames per transaction, so their geometry is unchanged.

The grouped unpublished-prefix gates, original-adapter regression campaign,
workload/telemetry helper tests and runnable manual example were executed.
This benchmark changes no production Rust, public API or existing file format.
It does not replace PR #18's safety checks or main-versus-PR regression study.
OOM/kill counters remained at their pre-existing six/three values through
the retained campaigns. This is not an OOM-impossibility proof.
No merge or release is performed.

See [local validation](validation.json), [environment](environment.json),
[evidence manifest](evidence-manifest.json),
[retained evidence archive](evidence.tar.gz) and [checksums](SHA256SUMS).
Full database payloads are omitted from Git; retained failed payload compression
identities are recorded in the original attempt metadata.
