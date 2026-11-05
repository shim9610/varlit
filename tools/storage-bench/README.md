# Workload-specific storage comparison

This standalone diagnostic crate compares Varlit, SQLite and HDF5 using the same
logical data and successful operations. It changes neither Varlit's public API nor
its file format. It is deliberately outside the normal Cargo workspace; native
SQLite and HDF5 installations are not requirements for library users.

Operation ratios report SQLite elapsed time / Varlit elapsed time for equivalent
work. Writes and reads are reported separately, as are candidate implementations
and configurations. Preparing this harness or passing its smoke tests verifies
operation contracts, not performance. The candidate grid does not exhaust every
possible engine-specific optimization.

## Scenarios and storage

### Grouped items: physical layout and publication are separate

Use `--suite grouped` for small logical items packed into application blocks.
The old `objects-*` profiles deliberately retain one physical Frame and disk
key per item; their `push_iter` calls coalesce writes but do not pack those
items into one record. They are individual-record controls, not the default
representation of grouped acquisition data. One million records does not imply
one million transactions: the old full fixture published 1,000 records per sync.

The grouped fixture uses 128-byte logical items with dense immutable u64 IDs.
ID / items-per-block identifies the block; ID % items-per-block identifies the
slot. A physical Frame contains 512 or 8,192 items (64 KiB or 1 MiB), with one
disk key per block. The primary publication contains 131,072 logical items /
16 MiB, independent of physical block size. The 128 MiB workload has 1,048,576 items, 2,048/128
physical blocks and eight publications. The 1 GiB workload has 8,388,608 items,
16,384/1,024 physical blocks and 64 publications. Producers partition whole
blocks without reusing a published block ID.

In runner configuration, `records` counts physical Frames, `group_items` counts
logical items per Frame, `payload` counts encoded payload bytes per Frame and
`per_tx` counts Frames per publication. Reports expose logical counts separately.

Point work is **group-local batched item lookup**, not unrelated cold random
gets: for each sequentially consumed block, execute a separate backend block
lookup, then copy/verify each item in reverse slot order through a bounded
block buffer. The scan's payload is not returned as the point result. Varlit
uses its disk index; SQLite uses a keyed BLOB table; HDF5 persists a dense group
directory carrying payload extents, and queries that directory for point reads.
Every engine pays for block retrieval, checksums where configured, allocation,
item extraction, caller-owned 128-byte copies and full validation. The returned
block is retained only for its item batch. No million-key RAM map is built.
This routing contract does not replace a general arbitrary-key index: an
application lacking a derivable/persisted block route must build and charge one.
A separate 1 GiB/four-producer/eight-reader contrast uses 8,192-item / 1 MiB
publications with both block sizes and all three keyed engines. Block keys
retain natural group-ID order rather than an artificial scattered hash.
The individually framed control uses the same item bytes, publication size,
reader count and logical IDs; its record/key granularity is intentionally different.

Mixed Metadata and Head records are retained. Multiple producers use bounded
MPSC for Varlit/HDF5 or independent SQLite connections; all engines still have
one active file-writing transaction at a time. Reader buffers/caches remain
private. No resource budget or ReadLimits is increased.

```sh
python3 tools/storage-bench/run.py --suite grouped-smoke --repeats 1 --probe --output /absolute/fresh/grouped-smoke
python3 tools/storage-bench/run.py --suite grouped-tuning --repeats 3 --output /absolute/fresh/grouped-tuning
python3 tools/storage-bench/run.py --suite grouped --repeats 3 --probe --tuning-results /absolute/fresh/grouped-tuning --output /absolute/fresh/grouped
python3 tools/storage-bench/run.py --suite grouped-control --repeats 3 --probe --output /absolute/fresh/individual-controls
cargo run --release --locked --manifest-path tools/storage-bench/Cargo.toml --example grouped
python3 tools/storage-bench/analyze_grouped.py --evidence-root /absolute/fresh --grouped-cohort grouped --control-cohort individual-controls --output /absolute/fresh/report
```

Tuning freezes a complete SQLite page/mmap choice per block size using the
128 MiB, one-producer/eight-reader fixture. Transfer to other sizes/concurrency
is disclosed, not claimed optimal. Both Varlit block sizes and the HDF5 sealed
chunk layout remain separate candidates. Individual controls use the explicitly
recorded default page/mmap settings, rather than pretending the grouped BLOB
tuning selects an optimal per-item SQL schema.

`telemetry` records common-clock writer-active bounds, per-publication write
call start/return bounds and reader consume intervals. A consume interval is
classified as entirely during writing only when both endpoints fit the writer
window; crossing intervals remain separate. Publication happens inside a write
call, so consume-helper completion delay is reported as bounds using that call's endpoints,
not an invented exact commit timestamp or an individual item API latency. Reader completion includes verification
and the entire item-query batch. These finite maximum-throughput runs do not
establish fixed-rate acquisition deadlines or long-duration losslessness.

The [canonical combined report](../../docs/performance/workload-storage-20261104/README.md) and existing dashboard integrate these measurements with the mixed/array/table cases. [Grouped cohort details](../../docs/performance/grouped-storage-20261104/README.md) are an appendix, not a separate comparison. The original `smoke`/`pilot`/`full` grids remain reproducibility/regression grids; do not publish their individual-record cases alone as the corrected grouped-item comparison.

| Scenario | Logical work | Varlit candidates | SQLite | HDF5 |
|---|---|---|---|---|
| Mixed logging | Declared fixed Metadata and variable Frame blocks, varying payload lengths, durable batches, one producer/writer and one/eight followers | Batch stream and general composite-key disk index; persistent typed cursors and follow | Type-specific tables, sequence primary keys, composite Frame index only for keyed queries, prepared statements, WAL/FULL | Compound Frame headers and Metadata, a byte payload dataset, batched hyperslabs, SWMR |
| Historical individual-record stress | 100,000/1,000,000 individually framed records; follow-only and arbitrary composite-key lookup are separate profiles | Bounded disk-index cache; stream for follow-only | Complete indexed schema for both writes and reads | Follow-only; arbitrary composite-key lookup explicitly unsupported |
| Numerical arrays: HDF5-favorable | Regular dense time axis, 16 f64 channels/sample; write, scan, one-sample points and 4,096-sample windows | Native buffered/mapped block views, stream, disk-indexed chunks, bounded owned matrix reads and immutable mmap matrix | Typed 16-REAL-column table and large BLOB chunks with incremental BLOB reads | Native f64 two-dimensional contiguous/chunked datasets, bulk writes and hyperslab reads; optional Fletcher32 |
| Transactional tables: SQLite-favorable | Independent 64-byte business rows, bounded dense IDs partitioned by producer, multirow insert transactions, full follow plus primary-key lookup of every row; one/four producers and eight followers | General-key index, row-cell matrix and transaction-aligned 100-row matrix candidates; multiple producers hand off to one writer | Typed columns, integer primary key, sequence index required for follow, independent writer connections, WAL/FULL | Compound dense-ID row axis, append-only sequence-ID dataset, SWMR; multiple producers hand off to one writer |

One Frame is an application block, **not one numeric sample**. The array workload
uses 64 KiB/1 MiB groups containing 512/8,192 complete samples. The historical
objects fixture uses individual records as an explicit stress
control. It does not establish an optimal encoding of independent entities.
The corrected grouped fixture preserves item IDs while amortizing framing and
indexing. No dataset or table is created per item.

This is an insert/append comparison. General SQL joins, secondary predicate
queries, competing updates, deletion, update history and crash recovery are not
implemented by this suite. Do not infer equivalent functionality for them. The
SQLite table scenario exercises concurrent writer connections and multirow
visibility, not arbitrary SQL functionality or crash-equivalent ACID in other
engines. SQLite itself permits one active write transaction per database.

## Concurrency and publication

Every handle stays on its owning thread/process. The Varlit/HDF5 writer receives
transactions through a **bounded multi-producer, single-consumer (MPSC) queue with capacity two**. The total number
of in-flight transactions also includes one per producer and the active writer.
CPU, queue blocking, input generation, coalescing copies and handoff are charged.
The writer never waits for reader acknowledgments in timing runs. Varlit
index/table-matrix profiles call their existing explicit compaction API after a
published batch when the file exceeds max(512 MiB, twice its last compacted size).
The entire compaction and old-reader file retention are charged. This is harness
maintenance policy, not a new library default or an increased resource budget. SQLite's
multi-producer table profile uses separate native connections and database-native
transaction serialization, without an added Rust-wide writer mutex.

SQLite's compile-time thread setting is recorded and a THREADSAFE=0 build is
refused. Connections explicitly request SQLITE_OPEN_FULLMUTEX (serialized mode).
This does not make concurrent write transactions possible. Varlit/HDF5 readers
run in independent processes. In particular, HDF5's thread-safe build may serialize
API calls with a global lock; separate SWMR processes avoid that benchmark artifact.

An application Head defines the committed prefix across logical block kinds.
Readers retain handles/cursors and consume only through a captured Head. They may
skip intermediate refresh generations, but must consume every published record
exactly once. Unique appended values remain immutable, so a later root still
contains the captured prefix. This is **prefix clamping**, not a claim that
separate Varlit API calls capture one atomic root or that HDF5 provides SQL MVCC.
SQLite reads each captured prefix inside one read transaction.

HDF5 flushes data before appending a checked commit marker, then flushes and
fsyncs. Markers identify logical counts and payload extent; partially visible
markers do not authorize additional reads. Published data chunks are sealed:
compound/ID chunks match a transaction batch, metadata chunks contain its two
metadata records, and byte payloads are padded to a chunk boundary. Padding and
copy costs are included. The sealed-chunk adapter requires complete transaction
batches; unsupported shapes fail explicitly. The table ID partition ensures each
transaction owns complete dense-ID chunks. Raw HDF5 datasets can expose staged
bytes; consumers must obey this application publication protocol.

SQLite WAL/FULL, Varlit sync/checkpoint and HDF5 flush/fsync have different recovery
contracts. This suite does not assert crash-recovery equivalence. Integrity
variants are labeled: HDF5 plain versus Fletcher32, SQLite's normal integrity
behavior, and Varlit CRC verification. Matrix mmap is used only after every writer
is closed. Matrix compaction rebuilds its generation companion; it does not fold
COW pages into the primary. Mmap may refuse a 1 GiB overlay under the unchanged
materialization limit; those refusals remain failures, and bounded owned bulk
reads are a separate candidate. No limits are raised to enable mmap.
No ReadLimits are increased. Table matrix reserved capacity, COW files and index
sidecars count toward storage. No live file is mapped under an immutability promise.

## Build and run

For the current small-item comparison, use the grouped commands and integration
step above. The original grids below remain historical reproduction/regression
commands; their standalone output is not the corrected combined report.

Linux, Rust 1.95+, a C compiler, Python 3, and native SQLite/HDF5 1.14 libraries
are required. The build script finds versioned shared objects without modifying
system directories. Set VARLIT_BENCH_NATIVE_LIB_DIR for a custom library directory.

```sh
cargo build --release --locked --manifest-path tools/storage-bench/Cargo.toml
python3 tools/storage-bench/run.py --suite smoke --repeats 1 --probe --output /absolute/fresh/smoke
python3 tools/storage-bench/run.py --suite tuning --repeats 3 --output /absolute/fresh/tuning
python3 tools/storage-bench/run.py --suite pilot --repeats 3 --probe --tuning-results /absolute/fresh/tuning --output /absolute/fresh/pilot
python3 tools/storage-bench/run.py --suite full --repeats 3 --probe --tuning-results /absolute/fresh/tuning --output /absolute/fresh/full
```

Run in an existing cgroup limited to **4 CPU / 16 GiB**, with the existing **32 GiB
storage budget**. The runner restricts affinity to at most four available CPUs,
refuses a memory cgroup exceeding 16 GiB, and refuses to start a profile when less than 3 GiB remains. A uniform 100 ms
free-space watchdog stops an active process group before the last 1 GiB is consumed.
It does not change cgroup/resource limits. Ensure enough free storage for one
profile's peak, including transient compaction copies. A refusal or failure is
retained; it must not be silently replaced by a larger budget.

Smoke includes staged-but-unpublished prefix gates. These deliberately wait for
reader observations and are **not timing observations**. Pilot uses 256 MiB arrays
and mixed logging plus 100,000 objects/rows. Full adds 16 MiB/256 MiB/1 GiB arrays,
64 KiB/1 MiB groups and mixed payloads, and 1,000,000 independent objects/rows.
Use --filter with a profile-name substring to inspect or rerun a frozen subset.
Run directories must be fresh. Successful workload files are removed after their
measurements; --keep-data retains them. Failed files/logs are preserved; large
allocated engine files are losslessly gzip-compressed between timed child runs,
with decompressed SHA-256 verification before deleting the uncompressed copy.
Sparse engine files may remain uncompressed. Compression is evidence housekeeping,
excluded from child timing/CPU/resource metrics. If a run stops at the disk
preflight, use the same arguments plus --resume after reclaiming space through
this lossless housekeeping. Resume verifies identical benchmark binaries and the
frozen configuration list, retains the original manifest/source snapshot, and
records a separate resume manifest. Completed failed attempts are not erased or
rerun into successes. Resume continues at completed-attempt boundaries; a partial
attempt without a result is retained and requires a fresh output directory.

The profiles in run.py freeze complete candidate layouts before measurement.
SQLite page/cache/mmap and HDF5 chunk/cache choices are recorded. The tuning suite compares 4/16/64 KiB SQLite pages and 0/1 GiB mmap,
including both 64 KiB/1 MiB BLOB groups. --tuning-results freezes one complete
configuration per family by median overall completion after at least three
observations per candidate. The 256 MiB/64 KiB mixed-block settings transfer to
the larger primary sizes and 1 MiB payloads; those transfers are not separate
optimality proofs. Layout alternatives for Varlit/HDF5 remain separate reported
profiles. These defaults are an initial comparison grid. Before a primary
performance claim, run a separate schema/configuration tuning study, preserve all
candidates, and freeze complete winners supporting the required queries. Do not
combine one schema's write speed with another schema's read speed as one engine.
No benchmark below is a replacement for PR #18's safety tests or main-versus-PR
performance regression measurements.

## Measurements and limitations

Each reader validates all payload bytes/fields, duplicate/missing records,
metadata membership and exact query results. Timing totals include verification;
phase counters expose input/backend work where available. Every reader processes
the same logical workload. Each profile uses fresh files, then warm page cache for
reads; there is no privileged cache eviction. Array single-reader point/range
results are separate from its full scan. Stream has no point/range operation:
compare only its common scan/write metrics, never its shorter total query workload.

The runner records source/binary/lockfile hashes, configurations, native versions,
commands, exit status and original stdout/stderr. A small C exec/wait4 wrapper
avoids inheriting Python's RSS high-water before exec. CPU includes waited-for
child processes. wait4 max RSS is a maximum, **not summed physical memory**.
Per-thread counters overlap (the coordinator owns the serialized writer); use
wait4 for total CPU rather than summing overlapping phase counters.
Separate --probe runs sample aggregate process-tree resident set size (RSS) and proportional set size (PSS), logical/allocated
files, WAL/SHM, TMPDIR and deleted-open allocated files. They also capture cgroup memory.current and file/anonymous memory, including
otherwise unmapped file cache. Cgroup measurements include the rest of this shared
environment; their pre-run baseline is retained and they are not attributed wholly
to one engine. Sampling is normally every 50 ms, with 5 ms startup discovery and fast-case sampling;
it can miss shorter peaks and transient files. Probes are not timing samples.

Reader metrics include open, refresh, consumption, CPU, poll count and maximum
observed pending record count. That count is not a precise publication-to-consume
latency distribution: generation skipping and fixed 100 us idle polling affect it.
Mixed/table writer completion and reader drain are separate. Initialization and
final close/checkpoint are separate and also included in overall completion.
Resource probes report peaks; final storage includes only engine files. Reader order checksums are checked against the actual writer commit order,
independently of the membership oracle. These checksums are regression checks,
not collision-free formal proofs. No universal speed,
real-time ingestion guarantee, or OOM impossibility follows from these measurements.

## Packed business-row supplement

The `packed` suite compares transaction-aligned 100-row Varlit matrix cells with
the frozen typed SQLite table at 100,000/1,000,000 rows, one/four producers and
eight readers. Row fields use a declared 64-byte little-endian layout inside each
6,400-byte cell. Dense bounded IDs address a cell and row offset; arbitrary-key
lookup is not implemented by this layout. Each point performs a real matrix read
and integrity check, even for a just-scanned row; there is no application result
cache. Packing/copying and maintenance are included. Full-batch and aligned ID
preconditions are checked. This adds a batching candidate rather than assuming
that one physical cell per business row is the best use of a block format.

```sh
python3 tools/storage-bench/run.py --suite packed --repeats 3 --probe --tuning-results /absolute/fresh/tuning --output /absolute/fresh/packed
python3 tools/storage-bench/analyze.py /absolute/fresh/full --output /absolute/fresh/report
```

Analysis reports full write cost (initialization + ingest + finalization), ingest
alone, reader-active work and overall completion separately. Array write timers
already include create/close/checkpoint. No required maintenance is dropped from
the full write-cost comparison.

The `hdf-mixed` supplement compares the original 1 MiB payload chunks with chunks
matching the smallest variable payload (16/256 KiB), for 64 KiB/1 MiB Frames and
one/eight followers. The fitted choice seals every payload extent without padding;
its additional chunk metadata and I/O calls remain charged. Both complete layouts
are retained rather than assuming either chunk geometry is universally fastest.

Array block size is a physical layout choice, not a logical query boundary.
Operation ratios therefore use the fastest complete measured SQLite candidate
across both BLOB block sizes and the typed-column schema at the same logical
array size. Each ratio names its baseline; no imaginary layout combines their
individual best times. Mixed Frame sizes remain distinct logical workloads.

The `mapped-arrays` supplement uses Varlit's native payload mapping with typed
block windows after every writer has closed, and the frozen SQLite BLOB layouts.
Files remain immutable for the full mapping lifetime. Native payload mapping
avoids matrix COW materialization and reuses the mapping's verified payload
certificates for repeated reads. Initial open/map/CRC cost is retained; no
verification or read limit is disabled. The primary typed-column SQLite results
remain available when selecting the fastest complete array baseline. This route
is not used in concurrent append/follow workloads.

Mapped-array profiles execute an initial and a repeated full scan, then the same
point/window queries for both engines. Both scans verify every value. Repeated
scan cost is separate from open and initial CRC verification. Point/window ratios
use SQLite observations with the same two-scan preparation; initial scan/write
ratios can use all complete layouts for the same logical samples. Do not compare
two-scan overall completion with the primary one-scan totals. The array CLI's
readers=0 means no *additional* readers; its main reader always performs the work.

The observer discovers child processes from proc/stat PPid relationships, since
some kernels omit proc task children files. PID birth times reject reused IDs.
Both mixed/data and direct array file paths are counted. The resource-only
reprobe.py tool checks frozen binary hashes and participant coverage; its telemetry
qualification is separate from whether an engine operation succeeds.

Build the integrated view after both cohort analyses (raw source exports remain unchanged):

```sh
python3 tools/storage-bench/integrate_report.py --original docs/performance/workload-storage-20261104/comparison.json --grouped docs/performance/grouped-storage-20261104/comparison.json --output docs/performance/workload-storage-20261104
```


## Independent read-validation comparison

`compare_read_validation.py` runs the same grouped/mixed/array workloads against
frozen pre-change and new binaries, all four read modes, and a pre-security main
snapshot. It includes identical-binary A/A controls, unpublished-prefix gates,
three balanced timing repetitions, and separate process-tree resource probes.
Builds/tests must finish before timing. Resource caps remain 4 CPU / 16 GiB /
32 GiB. `--smoke` is only an operation/visibility check, never a timing sample.

The new harness selects **reader-only** policy through
`VARLIT_BENCH_READ_VALIDATION=legacy|full|crc-only|structure-only|trusted`.
Writers, stored CRCs, grouping, queries and application byte oracles remain
identical. Unknown selections fail. The driver records the selection and binary
hash on every row; pre-change binaries run only their legacy policy.

```sh
python3 tools/storage-bench/compare_read_validation.py --before /path/to/before-binaries --after /path/to/after-binaries --pre-security /path/to/pre-security-mixed --output /path/to/new-results
```

The `read-validation` feature belongs only to this diagnostic harness and is
on by default. Disable it when building the same harness against a pre-policy
runtime; the compatibility helper accepts only `legacy`. Build the pre-PR main
snapshot's `mixed` binary only, since its payload mapping API predates the array
adapter's newer indexed block accessor. Production dependency features remain
`integrity,mmap` in every build.

Visibility gates use the **actual previous publication count**, not a producer's
transaction ID. A bounded MPSC can deliver source 1 before source 0, so ID zero
is not evidence that nothing has been published. The gate pauses the first
actual publication and still requires all readers to observe the empty prefix.
It is disabled for timing and resource observations.

The canonical dashboard also accepts `--read-validation docs/performance/read-validation-20261104/comparison.json` in the `integrate_report.py` command. This keeps the 69 mode configurations in the existing report as a separate cohort; it never pools their timings or invents new SQLite ratios.

After freezing `payload` and `writer` diagnostic binaries from the same harness for each supported snapshot:

```sh
python3 tools/storage-bench/analyze_read_validation.py /path/to/primary-results --output docs/performance/read-validation-20261104
python3 tools/storage-bench/diagnose_read_validation.py --before /path/to/before-binaries --after /path/to/after-binaries --output /path/to/new-diagnostics
python3 tools/storage-bench/analyze_read_validation_diagnostics.py /path/to/new-diagnostics --output docs/performance/read-validation-20261104/diagnostics.json
```

The diagnostic cohorts contain six balanced timing repeats, separate probes, a captured-payload API timer with the full byte oracle charged separately, a writer-only phase with checked post-write readback, and a longer 2,048-window range control. Primary and diagnostic cohorts stay separate. The final calibration rejects a 5% qualification if either baseline's repeat spread exceeds 5%; single resource probes cannot qualify memory/storage regressions. A/A-only statuses and every observed exceedance remain exported. See the linked measurement appendix for instrumented stage attribution; those tracer/timer observations do not replace the original timings.
