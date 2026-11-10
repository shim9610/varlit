# Local storage benchmark harness

This standalone diagnostic crate compares Varlit, SQLite and HDF5 using matched
logical work. It is outside the Cargo workspace; native SQLite and HDF5 are not
requirements for library users. Published cloud measurements and their generated
reports have been removed. Performance conclusions are pending a controlled
local rerun. This directory retains harness code, analysis tools and a report
template, without recorded timing or resource results.

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
  transactions and primary-key queries. Varlit includes row-cell and packed
  transaction-cell candidates. This case is distinct from grouped acquisition.
- Individual-record profiles remain explicit stress controls. Do not use them
  as the recommended encoding or sole comparison for grouped logging.

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
The runner expects an existing cgroup capped at 4 CPU / 16 GiB and a 32 GiB storage
budget. It does not raise those limits. It restricts affinity to at most four
available CPUs, refuses profiles with less than 3 GiB free, and uses a free-space
watchdog with a 1 GiB reserve. Allow for compaction copies and retained reader
files. A refusal is a result; do not replace it silently with a larger budget.

## Local measurement

Freeze the workload, build options, native versions, budgets, repetitions and
comparison method before examining results. Keep timing and instrumented resource
probes separate. Include identical-binary controls and retain unsuccessful runs.
These example commands collect new observations; they do not supply a performance
claim or a completed qualification:

```sh
python3 tools/storage-bench/run.py --suite grouped --repeats 7 --probe --output /absolute/fresh/grouped
python3 tools/storage-bench/run.py --suite tuning --repeats 7 --output /absolute/fresh/tuning
python3 tools/storage-bench/run.py --suite full --repeats 7 --probe --tuning-results /absolute/fresh/tuning --output /absolute/fresh/full
python3 tools/storage-bench/analyze.py /absolute/fresh/full --output /absolute/fresh/report
```

The `full` grid includes individual-record controls; its standalone output is not
an adequate grouped-item comparison. Keep grouped workloads and table-oriented
adverse cases explicit. Compare write/read phases separately with equal query
work, reader counts and publication sizes. Do not combine the fastest phase from
several engine configurations into an imaginary configuration.

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

`analyze_grouped.py`, `analyze_read_validation.py` and `integrate_report.py` remain
available for newly collected, explicitly identified cohorts. The HTML file here
is a generator template containing `__DATA__`, not a published benchmark result.
Do not mix cloud observations with the future local cohort or publish performance
conclusions before reviewing its controls and resource measurements.
