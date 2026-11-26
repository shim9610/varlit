# Benchmarks: purpose, method and results

This page is the entry point for Varlit's performance numbers: why they are
measured, how, and what the latest runs found. The harness and its full
scenario definitions are in [`tools/format-bench`](../tools/format-bench/README.md).

## Purpose

1. **Know where Varlit stands, honestly.** Each competing format is measured
   on the workload it was designed for: HDF5 on chunked n-d arrays, SQLite on
   transactional point updates, Parquet on column analytics. Varlit is not
   expected to win those; the point is to know by how much it loses, and to
   find what to fix.
2. **Check Varlit's own cases.** High-rate typed streams with live followers
   (`mixed`, `mixed-live`) and bounded cells updated in place (`cells`) are
   what Varlit is for. There it must win on throughput and freshness while
   never showing a reader an inconsistent or torn state.
3. **Catch regressions between Varlit revisions.** `ab.py` runs two builds
   alternately on one machine and flags a change only when the run ranges do
   not overlap and the medians differ by 5 % or more.

Correctness is part of every number: each run is checked against an
in-memory oracle, and a mismatch fails the run instead of producing a figure.

## Method

| Scenario | Designed for | What is measured |
| --- | --- | --- |
| `grid` | HDF5 | 6-D f32 array with a growing time axis; cold full reads, time series, sub-cubes, ensemble means, write |
| `oltp` | SQLite | accounts with dense ids; 1000 durable transfers (total and p99), cold and warm point reads, branch sums, load |
| `oltp-sparse` | SQLite | the same accounts with sparse 64-bit keys |
| `analytics` | Parquet | append-only 12-column table; cold group-by and range queries, write |
| `mixed` | Varlit | three typed streams written flat out, four live followers; throughput, publish p99, inconsistent views |
| `mixed-live` | Varlit | the same in real time with a 10 ms freshness bound and every window durable; late windows, views seen before durable |
| `cells` | Varlit matrix | fixed 1 KiB cells updated in place, 64 per atomic update, four followers; update rate, follower lag, torn or corrupt reads |

Rules that apply to every engine:

- **Same durability.** A write counts only once it is durable, and every
  engine syncs at the same level, chosen with `--device-flush`: `full` (the
  default) flushes the drive cache (`F_FULLFSYNC` on macOS, SQLite with
  `fullfsync`), `os` stops at the operating system's sync for all of them.
  The two differ only on macOS.
- **Cold reads are cold.** The page cache is evicted before cold reads
  (`posix_fadvise` on Linux, `msync(MS_INVALIDATE)` on macOS, an unbuffered
  open on Windows); `varlit-format-bench evict-check` proves it on the host.
- **Real disks.** Runs use a real disk, never tmpfs; `--gate` repeats a run
  whose disk probe slowed down during it.
- **Medians of repeats**, with every raw record kept in `results.jsonl`.
- Numbers from different machines or operating systems are not comparable
  row by row; compare engines within one run, and revisions with `ab.py`.

## Latest results

### macOS, Apple M4, 1 GB per scenario (2026-11-27)

Mac mini M4 (10 cores, 16 GiB), internal SSD, APFS, 3 repeats, sidecar-free
streams. Full tables, conditions and the raw-data note:
[results-mac/20261127.md](../tools/format-bench/results-mac/20261127.md).

| Scenario | Best | Varlit |
| --- | --- | --- |
| grid | HDF5 (all reads 1.28 s, write 3.1 s) | 2.16 s reads, 4.4 s write: 1.7x HDF5 |
| oltp | **Varlit** (1000 txn 4.02 s, p99 5.1 ms) | SQLite 5.03 s, p99 43 ms; SQLite faster on cold points (1.67 vs 2.07 s) and branch sums |
| oltp-sparse | SQLite on transactions (4.78 s) | 5.63 s, lower p99 (26 vs 42 ms); slow load (9.8 s) |
| analytics | Varlit and Parquet level | Q1 0.256 vs 0.269 s; Q2 0.038 vs 0.034 s |
| mixed | **Varlit** (10.4x real time, 0 inconsistent views) | HDF5 level on throughput but showed 4 inconsistent views |
| mixed-live | **Varlit** among consistent engines (2.25 % late, 0 % seen before durable) | HDF5 1.35 % late but 100 % seen before durable and 4 inconsistent views |
| cells | **Varlit** (203 updates/s, 0 torn reads) | SQLite 141, HDF5 145 with about 1,780 torn reads |

All 78 runs passed the oracle. The page cache check went from 100 % to 0 %
resident. The disk probe (3.9 s per 1000 full flushes) exceeded the gate, so
the run was ungated. `grid` ran without a memory cap; macOS enforces none.

Removing the stream `.vks` sidecar (A/B on the same Mac, same harness): no
metric flagged. oltp and analytics are within 2 %; mixed (+97 %) and cells
(+43 %) medians improved, but their run ranges overlap, so the improvement is
not established. Varlit's `mixed` throughput varies from 3.2x to 10.4x real
time between runs on both revisions.

### Linux, 4 vCPU cloud VM, 10 GB per scenario (2026-11-18)

Measured before the sidecar removal:
[results-10gb-20261118.md](../tools/format-bench/results-10gb-20261118.md).
Varlit led oltp transactions (1.7x SQLite, p99 1 ms vs 20 ms) and mixed; HDF5
led grid; Parquet led analytics (Varlit Q2 33 % slower); SQLite led cells.

## Where Varlit is behind

- **grid:** reads and writes about 1.7x HDF5.
- **oltp cold point reads and aggregates:** SQLite is faster on both machines.
- **oltp-sparse load:** about 10x HDF5's load time.
- **mixed variance:** throughput swings by 3x between runs; the cause is open.

## Rerunning

```sh
cargo build --release --locked --manifest-path tools/format-bench/Cargo.toml
tools/format-bench/target/release/varlit-format-bench evict-check /abs/real-disk/dir
python3 tools/format-bench/run.py --data-gb 1 --repeats 3 --gate --out /abs/real-disk/dir
python3 tools/format-bench/report.py /abs/real-disk/dir
```

Add results for a new host under `tools/format-bench/results-<host>/` and a
row to this page.
