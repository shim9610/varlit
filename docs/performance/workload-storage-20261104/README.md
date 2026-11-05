# Varlit / SQLite / HDF5 workload comparison

This is the canonical combined report. The corrected grouped-item results are integrated below with the existing mixed logging, numerical-array and table-oriented cases.

The former individual-Frame million-item fixture is withdrawn as a representative grouped-storage model. Its claim that independent logical items should each be a separate physical Frame/index key was incorrect. Those measurements remain historical controls; their timings and storage sizes are not product-wide conclusions and are excluded from the dashboard's current view.

[Current integrated dashboard](index.html) · [Current integrated data and operation ratios](current-comparison.json) · [Grouping manual](../../scalable-io.md#choose-the-physical-block-before-choosing-the-batch-size) · [Runnable harness](../../../tools/storage-bench/README.md)

## Maintenance before/after comparison

The [maintenance qualification](../maintenance-20261106/README.md) compares the
0.13.0 cleanup with `9343ea1`, using matched release builds, unchanged budgets,
complete concurrent reader/query work and separate resource probes. Five-round
primary observations and fixed 30-round calibration are kept as separate cohorts.
The rejected Rust-module split and its 16–22% regression remain in the evidence.

| Workload | n | Completion change | CPU change |
| --- | ---: | ---: | ---: |
| 128 MiB, 1 producer / 1 reader, 1 MiB blocks | 5 | -2.74% | -3.89% |
| 128 MiB, 1 producer / 8 readers, 64 KiB blocks | 30 | -0.48% | +1.86% |
| 1 GiB, 4 producers / 8 readers, 1 MiB blocks | 30 | -1.46% | -1.19% |
| 1 GiB, 4 producers / 8 readers, 64 KiB blocks | 5 | -7.35% | -6.78% |
| 128 MiB native views, 64 KiB blocks | 5 | +2.01% | +2.89% |
| 128 MiB immutable mmap, 64 KiB blocks | 5 | +2.23% | +3.75% |

## Maintenance before/after comparison

The [maintenance qualification](../maintenance-20261106/README.md) compares the
0.13.0 cleanup with `9343ea1`, using matched release builds, unchanged budgets,
complete concurrent reader/query work and separate resource probes. Five-round
primary observations and fixed 30-round calibration are kept as separate cohorts.
The rejected Rust-module split and its 16–22% regression remain in the evidence.

| Workload | n | Completion change | CPU change |
| --- | ---: | ---: | ---: |
| 128 MiB, 1 producer / 1 reader, 1 MiB blocks | 5 | -2.74% | -3.89% |
| 128 MiB, 1 producer / 8 readers, 64 KiB blocks | 30 | -0.48% | +1.86% |
| 1 GiB, 4 producers / 8 readers, 1 MiB blocks | 30 | -1.46% | -1.19% |
| 1 GiB, 4 producers / 8 readers, 64 KiB blocks | 5 | -7.35% | -6.78% |
| 128 MiB native views, 64 KiB blocks | 5 | +2.01% | +2.89% |
| 128 MiB immutable mmap, 64 KiB blocks | 5 | +2.23% | +3.75% |

## Maintenance before/after comparison

The [maintenance qualification](../maintenance-20261106/README.md) compares the
0.13.0 cleanup with `9343ea1`, using matched release builds, unchanged budgets,
complete concurrent reader/query work and separate resource probes. Five-round
primary observations and fixed 30-round calibration are kept as separate cohorts.
The rejected Rust-module split and its 16–22% regression remain in the evidence.

| Workload | n | Completion change | CPU change |
| --- | ---: | ---: | ---: |
| 128 MiB, 1 producer / 1 reader, 1 MiB blocks | 5 | -2.74% | -3.89% |
| 128 MiB, 1 producer / 8 readers, 64 KiB blocks | 30 | -0.48% | +1.86% |
| 1 GiB, 4 producers / 8 readers, 1 MiB blocks | 30 | -1.46% | -1.19% |
| 1 GiB, 4 producers / 8 readers, 64 KiB blocks | 5 | -7.35% | -6.78% |
| 128 MiB native views, 64 KiB blocks | 5 | +2.01% | +2.89% |
| 128 MiB immutable mmap, 64 KiB blocks | 5 | +2.23% | +3.75% |

## Independent read-check modes: added comparison

The same report now includes [69 before/after/read-mode configurations](../read-validation-20261104/README.md) (207 timing observations, 69 separate resource probes, 48 publication gates), plus separately retained payload API, writer-only, range and memory diagnostics. Select **Independent read checks** in the dashboard. These medians form a separate cohort and do not replace or pool the cross-engine observations below.

CRC and additional structure checks are independently selectable, with the compatibility default preserved. TRUSTED uses captured metadata and skips frame rereads/data CRC while retaining bounds, allocation/decoder limits, finite traversal and publication guards. Direct captured-payload API observations are about 18% / 24% faster than FULL at 64 KiB / 1 MiB; this is not a universal speed claim. Optional record/cell CRC does not include required generation/commit checksums.

Five initial median-based alerts remain explicit, including a concurrent writer wall-time increase and native-owned range variation. Stronger repeat calibration leaves 79 of 88 metric comparisons unqualified and nine within the observed 5% threshold; it does not turn those alerts into passes. The appendix gives complete mode tables, CPU/memory/temp results, control ranges, failures and supplemental attribution.

## Grouped logical items: corrected primary model

Each logical item contains 128 bytes and has a dense immutable ID. A block contains 512 or 8,192 items; the block route follows from the ID and the item is a bounded slot inside that block. This is one declared item layout, not a million block types. Arbitrary keys lacking such a route require a separately charged routing index.

At 128 MiB there are 1,048,576 logical items, 2,048 or 128 physical data blocks, and eight 16 MiB publications. At 1 GiB there are 8,388,608 items, 16,384 or 1,024 data blocks, and 64 publications. Block packing, buffered append batches and publication are separate units.

Each independent reader follows every published block, validates all item bytes, separately fetches that block by its backend index/directory, and copies/verifies all its items in reverse slot order. The fetched block is retained only for that batch. These are full-follow plus group-local item queries, not unrelated cold random gets or latest-only reads. Reader-active time includes refresh, consumption and query verification, including tail work after writing.

### 128 MiB, one producer and eight readers

| Engine | Items/block | Full write s | Reader-active s | Complete s | CPU s | Final MiB | Aggregate PSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|
| varlit-indexed | 512 | 0.264 | 0.466 | 0.551 | 2.011 | 130.000 | 92.85 |
| sqlite | 512 | 0.553 | 0.525 | 0.720 | 2.285 | 146.754 | 158.23 |
| hdf5 | 512 | 0.406 | 0.843 | 0.945 | 3.454 | 128.203 | 170.99 |
| varlit-indexed | 8,192 | 0.279 | 0.670 | 0.753 | 2.681 | 128.242 | 122.95 |
| sqlite | 8,192 | 0.470 | 0.495 | 0.671 | 2.104 | 129.629 | 172.52 |
| hdf5 | 8,192 | 0.377 | 0.775 | 0.863 | 3.278 | 128.027 | 179.41 |

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

## Mixed-block logging

Eight readers each consume the entire published logical workload. A nominal 1 GiB maximum-size Frame set actually contains 767.969 MiB at the variable 64 KiB sizes, or 767.5 MiB at variable 1 MiB sizes. Physical frames and payload bytes are counted separately.

| Profile | Full write s | Reader-active s | Completion s | CPU s | Final MiB |
|---|---:|---:|---:|---:|---:|
| Varlit stream / 64 KiB Frames | 1.325 | 1.254 | 1.457 | 5.217 | 770.484 |
| SQLite / 64 KiB Frames | 2.834 | 1.707 | 2.869 | 8.097 | 887.816 |
| HDF5, 1 MiB chunks / 64 KiB Frames | 2.322 | 2.541 | 2.687 | 10.256 | 810.539 |
| Varlit stream / 1 MiB Frames | 1.415 | 1.230 | 1.517 | 5.259 | 767.789 |
| SQLite / 1 MiB Frames | 2.648 | 1.718 | 2.704 | 8.047 | 777.066 |
| HDF5, 256 KiB chunks / 1 MiB Frames | 2.051 | 2.257 | 2.379 | 9.113 | 767.617 |

Full write includes engine initialization, ingest, maintenance and final close/checkpoint. Reader-active time is the maximum reader's consumption plus refresh work, including validation and gets, excluding idle sleeps. Completion also includes reader startup/drain. These overlapping phase counters must not be added.

Against the matching tuned SQLite workload, the stream's full-write ratios are about **2.14× / 1.87×** for 64 KiB / 1 MiB Frames; reader-active ratios are **1.36× / 1.40×** and completion ratios **1.97× / 1.78×**. This is maximum-throughput append/follow, not a fixed-rate acquisition or real-time loss guarantee.

HDF5 chunk geometry is also tested: 16 KiB payload chunks eliminate padding for the 64 KiB variable-size model but add enough chunk work to be slower than 1 MiB chunks. For 1 MiB Frames, 256 KiB chunks generally improve the eight-reader result and eliminate padding. Both complete layouts, including metadata/copy costs, remain reported.

## Numerical arrays and immutable reads

The following 1 GiB profiles return identical caller-owned bytes. A point returns one 128-byte sample; a window returns 4,096 samples. All 16 channel values are checked. Array profiles have one main reader; readers=0 in the CLI means no additional readers.

| Profile | Full write s | Initial scan s | Repeated scan s | 4,096 points s | 128 windows s |
|---|---:|---:|---:|---:|---:|
| Varlit native payload mmap / two scans | 0.925 | 0.107 | 0.061 | 0.003 | 0.005 |
| SQLite 1 MiB BLOB / two scans | 1.920 | 0.138 | 0.087 | 0.008 | 0.005 |
| SQLite 1 MiB BLOB / one scan | 1.764 | 0.135 | — | 0.010 | 0.006 |
| HDF5 contiguous f64 / one scan | 0.909 | 0.072 | — | 0.028 | 0.006 |
| HDF5 1 MiB chunks + Fletcher32 / one scan | 1.454 | 0.508 | — | 1.726 | 0.087 |

Two-scan totals must not be compared with one-scan totals as equal work. Ratios identify the fastest complete measured SQLite candidate across physical BLOB sizes and the typed-column schema. Point/window comparisons match the one/two-scan preparation; write and initial-scan work precedes the extra scan.

For the 1 MiB native payload mapping, conservative SQLite/Varlit ratios are about **1.91× full write, 1.27× initial scan, 1.43× repeated scan, 2.68× points, and 0.97× windows**. Initial open/CRC work is retained separately; repeated reads use the existing mapping's validated certificates. This mapping is used only after every writer closes, with immutable backing files throughout its lifetime.

HDF5 contiguous f64 storage remains a strong bulk candidate. Plain HDF5, Fletcher32, Varlit CRC32 and SQLite WAL/FULL have different integrity/recovery contracts; this report does not make them equivalent. Primary HDF5 contiguous 1 GiB observations include substantial timing variation (about 1.071–7.192 s for one grouping), retained rather than attributed to an intrinsic format cost. A later identical-binary six-run control has A/B medians 1.177481/1.127446 s, a 4.44% difference; that single control does not qualify the whole grid.

## Table-oriented adverse case: 100-row publications

This separate table-oriented adverse case preserves 100-row publication and one backend point read per consumed row. It is not the grouped-item logging model above. The result below uses 1,000,000 rows, four producer connections/queues, eight readers and 10,000 logical transactions. Varlit's 100-row candidate stores each complete transaction in a 6,400-byte cell with a declared row layout. Each point still calls the real matrix read and integrity path; no application result cache returns a just-scanned row.

| Profile | Full write s | Reader-active s | Completion s | CPU s | Final MiB |
|---|---:|---:|---:|---:|---:|
| Varlit general-key index | 115.786 | 30.614 | 115.833 | 297.131 | 574.973 |
| Varlit matrix / one row per cell | 25.481 | 5.169 | 25.523 | 53.791 | 141.328 |
| Varlit matrix / 100 rows per cell | 18.500 | 4.416 | 18.533 | 40.835 | 146.730 |
| SQLite typed table | 11.780 | 2.421 | 11.815 | 25.318 | 70.648 |
| HDF5 compound dataset | 12.379 | 7.792 | 12.419 | 46.380 | 70.164 |

Packing reduces the Varlit completion from about 25.52 s for row cells to 18.53 s, but SQLite is about 11.82 s. The best tested Varlit table candidate is therefore still about **1.57× slower**, and has a larger final footprint. Whole-cell processing for a 64-byte point remains charged. This is an insert/follow/warm-primary-key workload, not every typical SQL workload.

## Memory and temporary storage in the corrected concurrent model

The following are separate qualified resource runs for the 1 GiB, four-producer/eight-reader grouped query case. PSS includes the full writer/reader process tree; each reader processes the entire workload. Temporary bytes are included in visible bytes. Deleted-open bytes are reported separately; independent maxima must not be added as simultaneous peaks.

| Engine | Items/block | Aggregate PSS MiB | Peak visible allocated MiB | Peak temporary MiB | Peak deleted-open MiB |
|---|---:|---:|---:|---:|---:|
| varlit-indexed | 512 | 169.11 | 1102.14 | 0.00 | 0.00 |
| sqlite | 512 | 171.15 | 2366.73 | 1195.79 | 0.00 |
| hdf5 | 512 | 236.92 | 1025.55 | 0.00 | 0.00 |
| varlit-indexed | 8,192 | 192.09 | 1026.19 | 0.00 | 0.00 |
| sqlite | 8,192 | 197.91 | 2091.49 | 1057.86 | 0.00 |
| hdf5 | 8,192 | 246.72 | 1024.14 | 0.00 | 0.00 |

Process memory is sampled and can miss short peaks. Mapped file pages contribute to RSS/PSS; unmapped OS file cache does not. Shared-environment cgroup file/anonymous memory and pre-run baselines remain in the data; they are not attributed wholly to one engine. The existing **4 CPU / 16 GiB / 32 GiB** limits and ReadLimits remain unchanged. Larger publication buffers consume more memory, as the publication-size table shows.

## Cohorts, methods and evidence

| Retained cohort | Configurations | Successful timing observations | Qualified resource probes | Refused attempts |
|---|---:|---:|---:|---:|
| Initial mixed/array/table/control measurements | 122 | 360 | 120 | 8 |
| Corrected grouped/control measurements | 47 | 141 | 47 | 0 |

The integrated dashboard shows corrected grouped results alongside the still-applicable mixed/array cases and explicitly labeled table-oriented adverse cases. Six original individual-record stress configurations and five newer individual-record controls are historical and hidden by default. Limit refusals remain visible. Source medians are never pooled across cohorts or query contracts; three observations per condition do not establish a 5% regression qualification or globally optimal layouts.

Varlit/HDF5 use bounded MPSC with one writer and one/four producers. SQLite uses independent writer connections with FULLMUTEX but one active write transaction per database. Eight-reader cases use independent processes, including HDF5 SWMR. Writer ingestion never waits for per-publication reader acknowledgments. HDF5 application publication, Varlit CRC/checkpoint and SQLite WAL/FULL retain their different integrity/recovery contracts.

Write-phase time includes initialization, ingest, maintenance and final close/checkpoint work. Complete includes reader startup/drain; counters overlap and must not be added. CPU is kernel user+system including waited-for reader processes. Detailed [corrected methods and evidence](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/performance/grouped-storage-20261104/README.md), [per-operation data](current-comparison.json), [resource CSV](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/performance/grouped-storage-20261104/metrics.csv) and [local validation](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/performance/grouped-storage-20261104/validation.json) are part of this same comparison.

No production Rust, public API or existing library file format was changed by the benchmark correction. This comparison does not replace the separate [main-versus-PR regression matrix](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/verification/full-performance-20261104/README.md). No merge or release is performed.

## Historical diagnostics and original evidence

The old individual-Frame tables and conclusions are removed from this report's representative results. For audit/reproduction only, the [original frozen report](https://github.com/shim9610/varlit/blob/1e770ce3a73d2f317e32f34d250bc44cd9c4a421/docs/performance/workload-storage-20261104/README.md), [original raw cohort JSON](comparison.json), [original operation-ratio export](operations.md), [original resource export](resources.csv), [evidence manifest](evidence-manifest.json) and [evidence archive](evidence.tar.gz) retain the observations and failure history. Those raw exports are historical, not the current representative view.

The corrected cohort's [measurement appendix](https://github.com/shim9610/varlit/blob/9343ea1754947d3af8e03dfa525d6ba5af856008/docs/performance/grouped-storage-20261104/README.md) retains the before/after controls and rejected/interrupted candidates, including HDF5 header-chunk/cache geometry correction. Both archives preserve failures instead of rewriting them into successful measurements.
