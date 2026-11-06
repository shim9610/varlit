# Varlit / SQLite / HDF5 workload comparison

This is the canonical combined report. The corrected grouped-item results are integrated below with the existing mixed logging, numerical-array and table-oriented cases.

The former individual-Frame million-item fixture is withdrawn as a representative grouped-storage model. Its claim that independent logical items should each be a separate physical Frame/index key was incorrect. Those measurements remain historical controls; their timings and storage sizes are not product-wide conclusions and are excluded from the dashboard's current view.

[Current integrated dashboard](index.html) · [Current integrated data and operation ratios](current-comparison.json) · [Grouping manual](../../scalable-io.md#choose-the-physical-block-before-choosing-the-batch-size) · [Runnable harness](../../../tools/storage-bench/README.md)

## Protected append-only mmap sessions

The new `mmap` facade keeps native writer exclusion alive through the last
token, reader or owned mapping. Admission remains unsafe because external
nonparticipants can bypass advisory locking; derived session mappings have
safe entry points. Matrix storage and editable conversion are excluded.
See the [API contract](../../api-reference.md#append-only-mmap-sessions).

The final implementation is compared with `a3a1e26` across 12 equal-work
conditions, seven fixed repetitions and 48 separate resource probes.
Earlier prototype and resource-options corrections each retain their own
336 timings and 48 probes. Two fixed 30-round calibrations of those earlier
phase alerts add 450 timings and 15 probes. All **1,458 timings and 159 probes**
remain separate in the dashboard’s **Append-only ownership** scope; they are
not pooled. The final cohort includes the metadata-only empty-snapshot
`Debug` refinement and was rebuilt and remeasured after executable text
changed. The earlier calibrations do not qualify that final binary.

This fixture uses one process, one active writer and independent reader
threads, unlike the existing SQLite live cohort’s reader processes. Every
live reader verifies all 1 GiB during append plus final drain. Warm readers
each scan all 128 MiB/1 GiB twice after writing, using one fixed snapshot.
There are no random queries. Blocks average 64 KiB or 1 MiB; 16 MiB durable
publications give 64 transactions and 16,384/1,024 data records per 1 GiB.
Four-producer cases use bounded MPSC, variable block lengths and two metadata
records per publication. Every metadata value and payload byte is checked.

### Final 1 GiB live measurements

Each pair below is **new ordinary mmap / new guarded mmap**, from the same
binary. The dashboard also retains both identical old-runtime A/B controls.
Writer-active includes input generation, queue handoff and publications;
reader-active is the maximum worker’s refresh plus verified reading, including
drain. Completion includes startup and cleanup; overlapping timers are not
additive. CPU is kernel wait4 user+system for all threads. During-write counts
only full consume intervals inside the writer window for the least-advanced
guarded reader.

| Blocks / readers / producers | Writer s | Reader-active s | Completion s | CPU s | Guarded during-write % |
|---|---:|---:|---:|---:|---:|
| 1024 KiB / 1 / 1 | 0.875 / 0.934 | 0.212 / 0.236 | 0.890 / 0.951 | 0.757 / 0.858 | 98.44 |
| 1024 KiB / 8 / 1 | 1.103 / 1.059 | 0.721 / 0.689 | 1.131 / 1.081 | 2.575 / 2.396 | 98.44 |
| 1024 KiB / 8 / 4 variable | 1.140 / 1.107 | 0.816 / 0.789 | 1.166 / 1.129 | 2.736 / 2.662 | 98.44 |
| 64 KiB / 1 / 1 | 1.288 / 1.284 | 0.629 / 0.647 | 1.301 / 1.298 | 1.532 / 1.543 | 98.44 |
| 64 KiB / 8 / 1 | 1.426 / 1.440 | 3.296 / 3.256 | 3.340 / 3.299 | 5.445 / 5.437 | 28.12 |
| 64 KiB / 8 / 4 variable | 1.447 / 1.444 | 3.316 / 3.330 | 3.369 / 3.379 | 5.448 / 5.515 | 28.12 |

All four read-check variants remain supported by the API; this performance
cohort holds FULL checks fixed. No read limit or 4 CPU / 16 GiB / 32 GiB budget
is raised. Mapping/unmapping, checksums, byte verification and fixed-snapshot
index copies remain charged. Native record-window follow stops at its last
cached record; it does not copy/revalidate the entire old prefix per poll.

### Retained alerts and fixed calibration

The prototype has warm reader-phase alerts of +7.03% (1 GiB/64 KiB) and
+6.46% (128 MiB/1 MiB), plus +5.41% writer time (1 GiB/1 MiB), all with eight
readers. The corrected-options cohort also observes live 1 MiB/eight-reader
CPU +6.39% and reader-active +12.31%, warm 1 GiB/64 KiB CPU +5.34% and
reader-active +6.23%, and 128 MiB/64 KiB writer-active +11.93%. These initial
medians remain visible. An identical old-runtime A/B CPU pair differs by
−10.70% in that cohort. No alert is converted to a pass by pooling repeats.

The fixed 30-round comparisons below use one binary’s raw A/B and guarded
routes, with all six execution orders occurring five times. Changes are
guarded / raw A; the last column retains the raw B / raw A CPU control.

| Condition | Writer change | Reader-active change | Completion change | CPU change | Raw B CPU change |
|---|---:|---:|---:|---:|---:|
| Warm 1024 MiB, 1024 KiB, 8 readers | +0.77% | +0.77% | +1.14% | +1.87% | -1.16% |
| Warm 1024 MiB, 64 KiB, 8 readers | -0.21% | +0.88% | +1.00% | +0.20% | +0.64% |
| Warm 128 MiB, 1024 KiB, 8 readers | -2.67% | +1.65% | -1.03% | -1.44% | +0.93% |
| Live 1 GiB, 1024 KiB, 8 readers | +0.40% | -4.55% | +0.41% | -0.50% | +0.28% |
| Warm 128 MiB, 64 KiB, 8 readers | +1.26% | +2.36% | +1.72% | +5.01% | -1.40% |

The short 128 MiB/64 KiB calibration retains a **+5.01% CPU** median.
The calibration therefore does not establish a blanket 5% bound, even though
the larger initial phase alerts do not persist at those magnitudes. The
final implementation’s own medians and repeat spreads remain separate.

The final seven-run cohort also retains alerts: guarded versus new ordinary
mmap at live 1 GiB/1 MiB/one reader is +6.85% completion and +13.32% CPU
(writer +6.72%, reader-active +11.34%). The 128 MiB/eight-reader warm cases
show +5.85% writer time at 1 MiB and +6.75% CPU at 64 KiB. Ordinary mmap also
shows +7.90% CPU versus old A at live 1 MiB/eight readers and +5.43% with four
producers. Identical old-runtime A/B controls vary by up to 9.17% completion
and 7.98% CPU in this final cohort. These controls leave attribution unresolved;
they do not erase the measured increases or establish a 5% regression pass.

### Memory, storage and evidence

Live sampled process PSS is 73.7–162.8 MiB ordinary / 73.8–161.5 MiB guarded.
Two-pass warm sampled process PSS is 145.7–1113.3 MiB ordinary / 133.4–1107.4 MiB guarded.

Every matched variant has identical final allocated storage; temporary and
deleted-open peaks are zero in the final probes. Warm mappings touch the
whole file, so residency can approach the dataset size. Probes sample every
50 ms and can miss brief peaks; file-cache/cgroup baselines and RSS are
exported separately and must not be added to PSS as independent residency.
No cohort records a new OOM or OOM-kill event. Four unchanged-path unit
controls also retain 84 timings and 12 probes in the archive.

The facade adds one fallible shared-protection allocation at admission and
shared ownership at token/map construction and release. Payload access and
ordinary append acquire no new lock or lease clone. This does not remove
existing mapping, validation and indexing costs or promise zero overhead.
Native regressions cover staged/confirmed visibility, rollback and failed
rollback, durability retry, aliases, writer-first drop, concurrent registration
and release, panic cleanup, partial resource defaults and all read policies.

The Windows CI follow-up corrects a test assumption about mapped-file rollback:
Windows refuses the file-length change with `ERROR_USER_MAPPED_FILE` (1224).
The tests require the typed rollback error, retained poison and confirmed bytes,
then release every protection before native reopen/recovery and append retry.
File leases without mappings separately require successful rollback/retry.
The fix changes tests, CI ordering and documentation only; the rebuilt release
executable's `.text` hash remains
`307d0834feb92f9bfbf97b215bb0bf6118c32fb40779c737ab71b5094374d330`.
The existing performance observations therefore retain their executable code.

[Raw observations, manifests, sources, patches and verification logs](append-only-mmap-20261106.tar.gz)
are covered by [checksums](CURRENT_SHA256SUMS). The [harness protocol and commands](../../../tools/storage-bench/README.md#append-only-ownership-comparison)
reproduce the cohorts. No new SQLite/HDF5 measurements or cross-engine ratios
are inferred from this ownership study.

## SQLite mmap and append-only readers

The 6 November recheck adds **714 timing observations, 102 separate resource
probes and ten publication gates** under the same 4 CPU / 16 GiB / 32 GiB caps.
Every configuration has seven fixed repetitions. The primary and live-snapshot
cohorts remain separate in the dashboard's **SQLite mmap recheck** view; their
minima, quartiles, maxima, configuration facts and raw evidence are retained.
HDF5 was not rerun in this recheck; its earlier measurements below keep their
original cohorts.

The previous immutable-array comparison already used Varlit mmap and SQLite's
1 GiB mmap cap. The previous grouped concurrent fixture used ordinary Varlit
indexed reads and SQLite mmap off, selected by its earlier separate tuning.
This recheck holds the 64 KiB SQLite page size and 8 MiB per-connection cache
fixed and measures mmap requests 0 / 1 / 2 GiB for every workload, using SQLite
3.46.1, serialized connections and WAL/FULL. The 2 GiB request is effectively
clamped to 2,147,418,112 bytes by this build. Process mapping probes confirm
primary database mappings for enabled configurations; a requested cap alone
would not establish that. Mappings do not establish which pages were served
through mmap, especially while reads can also use WAL.

A private append-only writer can coexist with both native record windows and
fixed confirmed-prefix snapshots. Every captured record stays immutable;
truncation, replacement, compaction and growing-matrix rewrites are excluded.
The existing unsafe APIs and validation defaults are preserved. The added
regression holds old borrowed slices across a staged append, checks that all
eight readers hide the uncommitted tail, then exposes the committed new record
without changing either old mapping.

### Static 1 GiB, 1 MiB blocks, eight scan workers

All rows write the same 8,388,608 128-byte samples in 1,024 physical blocks and
64 durable 16 MiB publications. After the writer closes, one reader performs
two complete scans, 4,096 seeded sample queries and 128 windows of 512 KiB;
eight independent reader threads each perform another complete scan. Write
includes initialization/final checkpoint and excludes input generation. Scan
below includes byte verification; repeated scan, point and window backend
timers are also exported separately. Completion and CPU include all work.

| Configuration | Write s | Initial scan s | Complete s | CPU s | Peak PSS MiB | Peak temp MiB |
|---|---:|---:|---:|---:|---:|---:|
| Varlit buffered view, FULL | 0.864 | 0.174 | 2.316 | 3.406 | 54.0 | 0.0 |
| Varlit mmap, FULL | 0.849 | 0.137 | 1.764 | 2.942 | 1122.3 | 0.0 |
| SQLite mmap off | 1.840 | 0.157 | 3.060 | 3.771 | 86.1 | 65.5 |
| SQLite mmap cap 1 GiB | 1.869 | 0.183 | 2.804 | 3.355 | 1060.8 | 65.5 |
| SQLite mmap request 2 GiB | 1.853 | 0.177 | 2.882 | 3.704 | 1096.1 | 65.5 |

The same static work is also measured at 128 MiB and with 64 KiB blocks. The
mmap routes touch the complete file during the parallel scans, so sampled
proportional set size (PSS) is about 1 GiB here. The buffered view has a smaller
resident footprint. Mapped virtual size is reported separately from residency.

### Live 1 GiB, one writer and eight independent reader processes

Each reader consumes and verifies the entire 1 GiB through a 4 MiB caller buffer
while the writer publishes every 16 MiB. There are no random lookups in this
sequential-sharing fixture. Writer-active includes input generation and
publication notifications, and excludes initialization/final checkpoint;
completion includes initialization, reader startup/drain and final checkpoint.
Reader-active is the maximum reader's refresh plus verified consumption work,
including any tail after writing. The writer does not wait for reader receipts.

**1 MiB physical blocks, live-snapshot cohort:**

| Configuration | Writer-active s | Reader-active s | Complete s | CPU s | Peak PSS MiB | Peak temp MiB |
|---|---:|---:|---:|---:|---:|---:|
| Varlit buffered view, FULL | 1.326 | 0.490 | 1.367 | 3.147 | 80.7 | 0.0 |
| Varlit record-window mmap, FULL | 1.299 | 0.463 | 1.339 | 2.700 | 76.7 | 0.0 |
| Varlit prefix-snapshot mmap, FULL | 1.276 | 0.474 | 1.315 | 2.659 | 105.2 | 0.0 |
| SQLite mmap off | 2.427 | 1.270 | 2.961 | 6.425 | 142.1 | 1045.2 |
| SQLite mmap cap 1 GiB | 2.356 | 1.269 | 2.823 | 6.369 | 142.2 | 1045.2 |
| SQLite mmap request 2 GiB | 2.367 | 1.246 | 2.830 | 6.406 | 142.2 | 1045.2 |

**64 KiB physical blocks, same cohort and logical work:**

| Configuration | Writer-active s | Reader-active s | Complete s | CPU s | Peak PSS MiB | Peak temp MiB |
|---|---:|---:|---:|---:|---:|---:|
| Varlit buffered view, FULL | 1.577 | 0.737 | 1.620 | 3.965 | 85.8 | 0.0 |
| Varlit record-window mmap, FULL | 1.771 | 0.986 | 1.829 | 4.793 | 85.5 | 0.0 |
| Varlit prefix-snapshot mmap, FULL | 1.904 | 1.601 | 2.040 | 6.830 | 213.1 | 0.0 |
| SQLite mmap off | 2.685 | 1.435 | 3.230 | 7.187 | 141.2 | 1182.4 |
| SQLite mmap cap 1 GiB | 2.632 | 1.427 | 3.221 | 7.248 | 141.2 | 1182.4 |
| SQLite mmap request 2 GiB | 2.673 | 1.488 | 3.191 | 7.390 | 141.2 | 1182.4 |

Record windows map once per block: 8,192 maps across eight readers at 1 MiB,
versus 131,072 at 64 KiB. The smaller-window route is slower than the buffered
view in this measurement. Prefix snapshots reduce map calls but copy the
complete captured index on each rebuild. At 64 KiB their median aggregate is
434 rebuilds and 3,196,566 copied entries across eight readers; at 1 MiB it is
512 rebuilds and 282,880 copied entries. That repeated prefix work is charged
and is not a constant-cost solution for indefinite streaming. The window and
buffered routes follow new chain entries from the last cached record.

For the 1 MiB rows the least-advanced reader completes 98.44% of its bytes in
consume intervals wholly inside the writer window. The 64 KiB snapshot row is
90.63%; the other 64 KiB rows are 98.44%. Crossing intervals are excluded.
Final allocated storage is about 1,024.1 / 1,033.3 MiB for Varlit / SQLite at
1 MiB blocks and 1,025.8 / 1,170.4 MiB at 64 KiB. SQLite WAL/SHM space is included
in temporary peaks. Shared OS page-cache and anonymous-memory counters, their
baselines, mapped extents and deleted-open space are exported separately; they
must not be added to PSS as an independent simultaneous resident peak.

All FULL rows retain additional structural checks and payload CRC. CRC_ONLY
rows, also measured/exported for every Varlit path, disable only the optional
additional structure checks. Required bounds, resource limits and publication
guards remain enabled in both. The primary live cohort (28 configurations) is
retained alongside the 36-configuration snapshot cohort, with its own seven
repetitions and references; their medians are never pooled.

### Grouped keyed queries with four producers and eight readers

The primary cohort also repeats the existing indexed query fixture at 1 GiB,
with both physical block sizes and the three SQLite mmap caps. It packs
8,388,608 logical items into 1,024 / 16,384 Frames, uses 64 publications, and
charges every reader for full follow plus separate group-local backend lookup
and reverse extraction of every item. Multiple producers feed one active
writer; this is distinct from the native sequential-sharing fixture above.

At 1 MiB blocks, Varlit FULL completes in 3.964 s with 15.518 CPU s; SQLite's
2 GiB-request configuration completes in 3.998 s with 12.923 CPU s. Reader-active
is 3.869 / 2.982 s, respectively. Completion is close and Varlit uses more CPU.
Only 18.75% of the least-advanced Varlit reader's bytes finish wholly during
writing, versus 98.44% for that SQLite configuration; final drain is included.
The dashboard retains all ten grouped configurations and their resources.

### Harness qualification and reproducibility

The unchanged runtime is `4b07249`; production changes in this recheck clarify
mmap safety documentation without changing executable library code. Before/after
harness controls use the old binary twice as A/B and the new binary, with the
same static workload and fixed repetitions. An initial 64 KiB mmap alert
(+6.72% wall time, with identical-binary B +6.46%) is retained. A separate fixed
30-round calibration gives -1.96% wall / -0.13% CPU; it does not replace the
initial observation. Final seven-round controls cover both block sizes and
buffered/mapped paths: changes are +1.14% to +2.36% completion and -1.87% to
+4.22% CPU. An identical-binary pair still differs by +8.09% in one final case,
so these controls do not establish a universal 5% bound. All three control
cohorts remain distinct (258 timing observations and 27 probes).

No new OOM or OOM-kill event occurs in either comparison or any control cohort.
Budgets and read limits are unchanged; only rebuildable compiler output was
reclaimed before running. Timing, resource probes and gates run separately.
One complete SQLite configuration is selected by median completion within each
cohort/workload for operation ratios; write/read/CPU values are not spliced
from different caps. Full source snapshots, binary hashes, failed smoke
attempts, controls, measurement rows and verification logs are packed into
[the mmap evidence archive](sqlite-mmap-20261106.tar.gz), covered by
[the current checksum manifest](CURRENT_SHA256SUMS). Commands and API choices
are in the [runnable harness](../../../tools/storage-bench/README.md#sqlite-mmap-recheck).

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

For the 1 MiB native payload mapping, conservative SQLite/Varlit ratios are about **1.91× full write, 1.27× initial scan, 1.43× repeated scan, 2.68× points, and 0.97× windows**. Initial open/CRC work is retained separately; repeated reads use the existing mapping's validated certificates. In that earlier cohort, mapping starts after every writer closes, with immutable backing files throughout its lifetime.

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
