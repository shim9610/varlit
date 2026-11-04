# Workload-specific Varlit / SQLite / HDF5 comparison

The requested **at least 2× SQLite performance for every equivalent operation is not established**. Large mixed-block writes approach or exceed that target in some configurations; following new data, array scans/windows and many small objects do not consistently do so. Packing business rows improves Varlit's result, but the fastest tested table candidate remains slower than SQLite.

This report contains **122 complete candidate configurations, 360 successful timing observations, 120 successful separate resource probes, and eight retained matrix-mmap limit refusals**. The refusals are six timing attempts and two resource attempts for two configurations. Three fixed repetitions are observational medians, not a 5% regression qualification or proof that these are globally optimal implementations. All original measurements, configuration/source identities and exclusions are retained.

[Interactive comparison](index.html) · [Machine-readable results](comparison.json) · [Operation ratios](operations.md) · [Resource table](resources.csv) · [Runnable harness and full contracts](../../../tools/storage-bench/README.md)

No production Rust, public API or existing file format changes were made for this study. It does not replace the separate [main-versus-PR regression matrix](../../verification/full-performance-20261104/README.md).

## Models and layouts

| Scenario | Equal logical work | Workload-appropriate layouts |
|---|---|---|
| Varlit-oriented mixed logging | Fixed Metadata and variable Frame declarations; 64 KiB/1 MiB maximum payload; one writer and one/eight followers | Varlit batched stream/general-key disk index; SQLite type-specific sequence tables with only required indexes; HDF5 compound headers/metadata, byte heap and SWMR publication markers |
| High cardinality | 1,000,000 independent 128-byte objects; follow-only and arbitrary composite-key gets measured separately | Varlit bounded disk index or stream; complete SQLite indexed/unindexed schemas as required; HDF5 follow-only, with unsupported arbitrary-key queries refused |
| HDF5-oriented numerical arrays | Dense regular time axis; 16 finite f64 channels per sample; 16 MiB/256 MiB/1 GiB; full scans, points and 4,096-sample windows | Varlit buffered/mapped native blocks, indexed chunks, stream and owned/mapped matrix candidates; SQLite typed REAL columns and BLOB groups; HDF5 native f64 contiguous/chunked hyperslabs, with plain/Fletcher32 variants |
| SQLite-oriented business tables | 100,000/1,000,000 independent 64-byte rows; 100-row insert transactions; one/four producers and eight followers; full follow plus a primary-key get of every consumed row | Varlit general-key index, dense row cells and 100-row cells; SQLite typed columns, integer primary key and sequence index; HDF5 dense-ID compound rows and sequence-ID dataset |

A 1 GiB array contains **8,388,608 logical samples / 134,217,728 scalar values**. It uses 1,024 Varlit blocks at 1 MiB, or 16,384 blocks at 64 KiB. SQLite may use sample rows or large BLOB groups; HDF5 uses a two-dimensional dataset. No numeric sample is forced into its own Varlit frame. The million-object and business-row cases are separate application entities.

Single-writer engines receive transactions through a bounded multi-producer, single-consumer queue of capacity two. Producer staging, handoff/blocking, copying, validation and maintenance remain charged. Writers never wait for reader acknowledgments in timing runs. SQLite uses independent writer connections with explicit FULLMUTEX, compiled THREADSAFE=1; one active write transaction per database remains its limit. Mixed/table readers use separate processes, including HDF5 SWMR, avoiding its process-local global API lock.

Readers clamp consumption to a captured application Head and validate every payload byte/field, duplicates/missing IDs, metadata and writer-derived within-type order checksums. These checksums are regression witnesses, not collision-free proofs. Values are immutable inserts; current queries use known published IDs. This does not establish negative-key behavior, update history, deletion, joins, secondary predicates or cross-call atomic roots. HDF5 publication is an application protocol, not SQLite MVCC/ACID.

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

Against the matching tuned SQLite workload, the stream's full-write ratios are about **2.14× / 1.87×** for 64 KiB / 1 MiB Frames; reader-active ratios are **1.36× / 1.40×** and completion ratios **1.97× / 1.78×**. A write improvement therefore does not satisfy the requested target for all reading work. This is maximum-throughput append/follow, not a fixed-rate acquisition or real-time loss guarantee.

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

## Business rows and many small objects

The business-table result below uses 1,000,000 rows, four producer connections/queues, eight readers and 10,000 logical transactions. Varlit's 100-row candidate stores each complete transaction in a 6,400-byte cell with a declared row layout. Each point still calls the real matrix read and integrity path; no application result cache returns a just-scanned row.

| Profile | Full write s | Reader-active s | Completion s | CPU s | Final MiB |
|---|---:|---:|---:|---:|---:|
| Varlit general-key index | 115.786 | 30.614 | 115.833 | 297.131 | 574.973 |
| Varlit matrix / one row per cell | 25.481 | 5.169 | 25.523 | 53.791 | 141.328 |
| Varlit matrix / 100 rows per cell | 18.500 | 4.416 | 18.533 | 40.835 | 146.730 |
| SQLite typed table | 11.780 | 2.421 | 11.815 | 25.318 | 70.648 |
| HDF5 compound dataset | 12.379 | 7.792 | 12.419 | 46.380 | 70.164 |

Packing reduces the Varlit completion from about 25.52 s for row cells to 18.53 s, but SQLite is about 11.82 s. The best tested Varlit table candidate is therefore still about **1.57× slower**, and has a larger final footprint. Whole-cell processing for a 64-byte point remains charged. This is an insert/follow/warm-primary-key workload, not every typical SQL workload.

For 1,000,000 independent small objects, the following workloads remain separate from grouped signal acquisition:

| Profile | Full write s | Completion s | CPU s | Final MiB |
|---|---:|---:|---:|---:|
| Follow-only / Varlit stream | 5.951 | 8.521 | 32.925 | 268.758 |
| Follow-only / Varlit disk index | 45.928 | 45.963 | 104.262 | 529.773 |
| Follow-only / SQLite, no unused key index | 2.154 | 2.197 | 5.220 | 148.895 |
| Follow-only / HDF5 | 1.518 | 1.564 | 5.742 | 152.750 |
| Follow + composite-key get / Varlit | 51.135 | 51.186 | 152.999 | 529.773 |
| Follow + composite-key get / SQLite | 4.487 | 4.523 | 15.393 | 169.629 |

The general-key route has substantial time/CPU/storage gaps. These observations do not prove an inherent lower bound or that further optimization is impossible. They do show that it would be incorrect to declare the requested target met, or to conceal these cases behind large-block throughput. Further attribution/optimization belongs with [issue #20](https://github.com/shim9610/varlit/issues/20).

## CPU, memory and temporary storage

The existing **4 CPU / 16 GiB / 32 GiB** limits were unchanged. Affinity uses four CPUs. CPU above is kernel wait4 user+system time including waited-for child readers. Per-thread phase counters overlap and are not summed to fabricate totals.

| Profile | Kernel max individual RSS MiB | Sampled aggregate PSS MiB | Peak visible allocated MiB | Peak temporary MiB | Peak deleted-open MiB |
|---|---:|---:|---:|---:|---:|
| Mixed 64 KiB / Varlit stream | 59.97 | 75.14 | 770.49 | 0.00 | 0.00 |
| Mixed 64 KiB / SQLite | 63.62 | 142.25 | 1795.54 | 907.71 | 0.00 |
| Mixed 1 MiB / HDF5 fitted chunks | 84.74 | 164.94 | 767.62 | 0.00 | 0.00 |
| 1M keyed objects / Varlit index | 25.41 | 95.88 | 851.40 | 77.20 | 515.09 |
| 1M keyed objects / SQLite | 16.43 | 45.87 | 910.13 | 740.50 | 0.00 |
| 1M rows, four producers / Varlit packed | 11.33 | 46.32 | 564.39 | 52.34 | 0.00 |
| 1M rows, four producers / SQLite | 19.95 | 34.96 | 537.55 | 465.86 | 0.00 |
| 1M rows, four producers / HDF5 | 13.41 | 40.50 | 70.16 | 0.00 | 0.00 |
| 1 GiB two scans / Varlit payload mmap | 1036.10 | 1035.94 | 1024.13 | 0.00 | 0.00 |
| 1 GiB two scans / SQLite BLOB | 1044.14 | 1036.34 | 1051.75 | 65.50 | 0.00 |
| 1 GiB one scan / HDF5 contiguous | 40.71 | 40.65 | 1024.01 | 0.00 | 0.00 |

Memory/storage values come from separate resource runs using each timing cohort's exact frozen benchmark binaries. RSS above is the median kernel maximum for one process, not an aggregate. PSS is a sampled aggregate across wrapper/writer/readers and is a lower bound on a potentially shorter peak. Most probes sample at 50 ms; fast cases use 5 ms and discovery retries. Samples can miss brief peaks; kernel max RSS remains available. The observer validates expected process coverage: ten processes for eight followers plus writer and wrapper, or two for a static array.

Mapped file pages contribute to process RSS/PSS; unmapped HDF5/buffered-I/O file cache does not. Cgroup memory.current and file/anonymous memory, with pre-run baselines, are retained to expose that distinction. They include the rest of this shared environment and cannot be attributed wholly to one engine. Do not infer a 25× physical-RAM disadvantage from a mapped profile versus HDF5's process RSS alone.

Temporary bytes are already part of visible allocated bytes. Deleted-open old generations are deduplicated by inode and reported separately. These are independent maxima, not a simultaneous total; adding them can overstate the true peak. Index/matrix compaction triggers at max(512 MiB, twice the previous compacted size), and all time/CPU and old-reader retention are included. This harness maintenance policy is not a new library cap. SQLite WAL/checkpoint costs are likewise retained.

## Refusals, corrections and evidence

- Two matrix-mmap 1 GiB configurations refuse their default 1 GiB materialization budget, producing six timing and two resource refusals. The 64 KiB case requests 1,073,815,552 bytes; the 1 MiB case also includes metadata above the limit. No limit is raised. Bounded owned matrix reads and native payload mmap successfully process the file through their different existing resource paths. These are allocation-limit refusals, not OOM-kill reproductions.
- The first unmaintained million-key index run exhausted available space in the existing shared 32 GiB filesystem. Its sidecar reached 10,601,758,720 bytes while other workspace data already occupied much of the volume. The append-only immutable index has an existing explicit compaction API; the final candidate invokes it and charges its cost. The failed sidecar is locally preserved as a 1,740,515,569-byte gzip with verified original SHA-256 b5084b20991b7932f8e5927888d6cb55d46c9e7bdda8aca74223314165d52ffc. It is not committed as a multi-gigabyte Git object; its failure/source/log/hash evidence is included.
- An initial HDF5 prototype rewrote published checksum chunks and failed concurrent reads. Transaction-aligned compound/ID chunks and sealed payload chunks corrected that publication protocol; padding is charged where needed. Forced unpublished-prefix and concurrent tests pass. Partial runs before the common idle-poll optimization were retained and not pooled into the final timing set. A later review corrected matrix maintenance to inspect its generation companion rather than the primary file; compaction does not checkpoint COW pages into the primary.
- The first resource observer silently saw only the wrapper because this kernel omits proc task children files, and missed array files stored directly in the workload root. Its sampler results are excluded. A corrected PPid/start-time observer and path accounting were checked against actual process/storage counts, then all 122 resource attempts were repeated with frozen binaries. A too-short 16 MiB mapping probe failed coverage qualification and was retained/repeated with faster sampling. CPU/timing observations were not rerun into different results to hide this correction.
- The first SQLite tuning schema carried an unused follow-only key index. Its 162 observations are retained but excluded from selection. The corrected 54-configuration grid has three successful observations each; complete page/mmap/layout winners are frozen before the cross-engine runs. Transfers from the 256 MiB/64 KiB tuning workload are disclosed in the harness documentation, not presented as global optimality.

Cgroup OOM/kill counters did not increase; historical counts were already six/three. This is an observation within this workload, not an OOM-impossibility proof. No merge or release is performed.

The evidence archive retains original sources, configuration manifests, binary identities, commands, stdout/stderr, successful/refused observations, corrected and excluded resource traces, tuning/control runs and prototype/interruption records. Full local engine databases are omitted from the Git archive; the locally retained failed index's hashes and round-trip verification remain recorded. See [evidence manifest](evidence-manifest.json) and [compressed evidence](evidence.tar.gz).
