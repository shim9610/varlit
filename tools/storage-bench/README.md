# Local storage benchmark harness

This standalone diagnostic crate compares Varlit, SQLite and HDF5 using matched
logical work. It is outside the Cargo workspace; native SQLite and HDF5 are not
requirements for library users. Measurement reports are not kept in the
repository. A comparison asks whether Varlit can reach performance comparable
with similar formats, not whether it is better; its SQLite and HDF5 code
follows the patterns the author knows to be efficient and does not mark those
libraries' limits, and a report states the machine it ran on. This directory
holds the harness, analysis tools and a report template.

## Scenarios

A scenario is defined by what the application needs: how much data, in what
units it must be committed to the file and shared with readers, and what it is
queried by. It never prescribes how an engine stores it. Each engine implements
every scenario its own best way, and each scenario states which engine its
workload is expected to favour. The `compare` suite holds them:

| Scenario | Requirement | Favours |
| --- | --- | --- |
| `mixed` | Fixed metadata and variable 64 KiB or 1 MiB frames in one log, 16 MiB publications, 1 or 8 followers | Varlit |
| `commit` | 128-byte units committed to the file and shared with readers every 1, 512 or 8,192 units, 1 or 8 followers | none |
| `grouped` | 1,048,576 items in groups of 512 or 8,192, one or four producers, 8 readers with group-local lookups | Varlit |
| `table` | 100,000 rows of 64 bytes, 100 per transaction, one or four producers, 8 readers with keyed lookups | SQLite |
| `array` | 256 MiB of 16-channel f64 samples in 1 MiB groups: write, scan, points, ranges | HDF5 |

How each engine meets a commit: Varlit publishes the commit's items as one
record (stream/indexed) or writes them into matrix cells; SQLite runs one
transaction per commit with typed tables or BLOBs; HDF5 appends the commit to
chunked datasets and flushes. No engine stores small items one record, row or
dataset at a time when the requirement lets it group them.

`compare-tuning` tries every SQLite candidate (page size 4/16/64 KiB, mmap off
or 1 GiB) and every HDF5 mixed/commit/grouped candidate (payload chunk automatic,
64 KiB, 256 KiB, 1 MiB) three times; `compare --tuning-results` freezes the
fastest complete configuration per profile and refuses to start if one is
missing. `analyze_compare.py` reports, per scenario and engine, medians with
min–max, CPU, memory and storage from the probe run, and for logged scenarios
commit latency and the time until readers see a commit.

## Workloads and representation

- Grouped logging packs 128-byte logical items into 64 KiB or 1 MiB application
  blocks, with 16 MiB publications. Logical items, physical records and publications
  are separate counts. A point operation retrieves a block through the backend,
  then copies and verifies its items in reverse slot order. It is group-local
  batched lookup, not unrelated cold random item lookup.
- Mixed logging declares fixed metadata and variable payload blocks. It exercises
  batched append, publication, following readers and optional keyed lookup.
- Numerical arrays use 16 f64 channels per sample and test bulk writes, scans,
  point queries and windows. A physical block contains many samples. HDF5 uses
  typed datasets; SQLite offers typed columns and BLOB layouts; Varlit offers
  native, stream, indexed and matrix candidates.
- The SQLite-oriented table case uses independent 64-byte rows, multirow
  transactions and primary-key queries. Varlit keeps them in matrix cells, one
  row per cell or a transaction's rows packed into one cell. This case is
  distinct from grouped acquisition.

Varlit and HDF5 use a bounded multi-producer, single-consumer queue and one writer.
SQLite uses independent writer connections with `SQLITE_OPEN_FULLMUTEX`, while
SQLite itself permits only one active write transaction per database. Followers
use independent processes. Queueing, copies, maintenance, validation and final
reader drain are part of the work. Timing runs do not wait for per-publication
reader acknowledgments.

SQLite WAL/FULL, Varlit publication/sync and HDF5 SWMR with application commit
markers have different recovery contracts. The harness does not establish
crash-recovery equivalence, arbitrary SQL functionality, real-time deadlines or
long-duration losslessness. Checksums and complete data/count/order oracles must
remain enabled as specified by each profile.

## Build and smoke checks

Use Linux, Rust 1.95+, a C compiler, Python 3 and native SQLite/HDF5 1.14 libraries.
Set `VARLIT_BENCH_NATIVE_LIB_DIR` for a custom native library directory. Build from
the checkout being measured and use fresh output directories outside the source
tree.

```sh
cargo build --release --locked --manifest-path tools/storage-bench/Cargo.toml --bins
python3 -m unittest discover -s tools/storage-bench -p 'test_*.py'
python3 tools/storage-bench/run.py --suite smoke --repeats 1 --probe --output /absolute/fresh/smoke
```

Smoke checks establish operation/oracle behavior, not performance qualification.
The runner requires an existing cgroup whose `memory.max` is at most 16 GiB and
records `cpu.max` and a nominal 32 GiB storage budget; the intended setup is
4 CPU / 16 GiB / 32 GiB. It does not raise those limits. It restricts affinity
to at most four available CPUs, refuses profiles with less than 3 GiB free, and uses a free-space
watchdog with a 1 GiB reserve. Allow for compaction copies and retained reader
files. A refusal is a result; do not replace it silently with a larger budget.

## Local measurement

Freeze the workload, build options, native versions, budgets, repetitions and
comparison method before examining results. Keep timing and instrumented resource
probes separate. Include identical-binary controls and retain unsuccessful runs.
These example commands collect new observations; they do not supply a performance
claim or a completed qualification:

```sh
python3 tools/storage-bench/run.py --suite compare-tuning --repeats 3 --output /absolute/fresh/tuning
python3 tools/storage-bench/run.py --suite compare --repeats 5 --probe --tuning-results /absolute/fresh/tuning --output /absolute/fresh/compare
python3 tools/storage-bench/analyze_compare.py /absolute/fresh/compare --output /absolute/fresh/report
```

Keep each scenario's favoured engine stated next to its results. Compare write/read phases separately with equal query
work, reader counts and publication sizes. Do not combine the fastest phase from
several engine configurations into one result.

The runner records source/binary hashes, commands, configuration, native versions,
stdout/stderr and failures. A C exec/wait4 wrapper avoids inheriting Python's
pre-exec RSS high-water. CPU includes waited-for children; maximum child RSS is
not aggregate physical memory. Separate probes sample process-tree RSS/PSS,
cgroup state, allocated files, temporary files and deleted-open storage. Sampling
can miss brief peaks, and cgroup observations include unrelated activity.

Reader-active phases overlap writer work and must not be added to completion.
During-write intervals and final drain are separate. Publication-helper timing
bounds are not exact item latency. Warm reads, preparation scans and mmap setup
must be identified when comparing results.

## SQLite mmap and read-policy comparisons

The focused mmap runner compares requested SQLite mmap ceilings and records
effective settings and observed mappings. Mapping extents are virtual coverage,
not resident memory or proof that every read used mmap. Static and live studies
remain separate; live snapshot rebuilding includes captured-index copy costs.

```sh
CARGO_TARGET_DIR=target cargo build --release --locked --manifest-path tools/storage-bench/Cargo.toml --bins
python3 tools/storage-bench/compare_sqlite_mmap.py --binary-dir target/release --smoke --output /absolute/fresh/mmap-smoke
python3 tools/storage-bench/compare_sqlite_mmap.py --binary-dir target/release --repeats 7 --output /absolute/fresh/mmap-primary
python3 tools/storage-bench/compare_sqlite_mmap.py --binary-dir target/release --live-snapshots --repeats 7 --output /absolute/fresh/mmap-snapshots
```

Varlit mmap requires immutable captured backing bytes for the full mapping
lifetime. Static matrix mappings require closed writers. Live native windows
require append-only writes outside the captured records, with no truncation,
rewrite or replacement. See [mmap safety](../../SECURITY.md#mapped-files-and-external-modification).

`compare_read_validation.py` accepts frozen `--before`, `--after` and
`--pre-security` binary directories, an `--output` directory and `--repeats`.
Use `--help` on the comparison and analysis scripts for their input contracts.
The `payload` and `writer` binaries support focused diagnostics. Retain data CRC,
structure checks and publication guards according to the selected profile;
independent policy comparisons do not make different integrity contracts equal.

`analyze_grouped.py` and `analyze_read_validation.py` remain available for newly
collected, explicitly identified cohorts. The HTML file here
is a generator template containing `__DATA__`, not a published benchmark result.
Do not mix cloud observations with the future local cohort or publish performance
conclusions before reviewing its controls and resource measurements.

Keep run output outside the checkout or under ignored `target/` / `results/`
directories. Logs, measurements, recovery archives and compiler extracts are not
versioned source. Store shared evidence in CI artifacts or external storage;
do not keep backup branches containing repeated archive snapshots. The obsolete
integrator for the withdrawn, dated cloud cohorts has been removed.
