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

### Linux, 4 vCPU cloud VM, 10 GB per scenario (2026-11-27)

Intel Xeon 2.10 GHz (4 vCPUs, 15.7 GiB), virtio disk, ext4, one run per
engine and scenario, ungated, sidecar-free streams. Full tables with the
previous pass beside them:
[results-10gb-20261127.md](../tools/format-bench/results-10gb-20261127.md).

| Scenario | Best | Varlit |
| --- | --- | --- |
| grid | HDF5 (all reads 1.20 s, write 19.9 s) | 1.57 s reads, 23.1 s write: 1.3x HDF5, level with SQLite |
| oltp | **Varlit** (1000 txn 0.46 s, p99 1.1 ms) | SQLite 1.25 s, p99 22 ms; SQLite faster on cold points (1.93 vs 2.30 s) and branch sums |
| oltp-sparse | Varlit and SQLite level (1.91 vs 1.80 s) | lower p99 (13 vs 24 ms); SQLite faster on cold points and sums |
| analytics | Varlit on Q1 (3.86 vs 4.14 s), Parquet on Q2 (0.067 s) | Q2 0.089 s: 1.3x Parquet |
| mixed | **Varlit** (4.58x real time, 0 inconsistent views) | HDF5 4.12x with 58 inconsistent views |
| mixed-live | **Varlit** among engines that never expose undurable data (5.4 % late, 0 % seen before durable) | HDF5 2.0 % late but 91 % seen before durable and 140 inconsistent views |
| cells | SQLite (373 updates/s) | 269 updates/s, 0 torn reads, 4.6x faster load; HDF5 3333 torn reads |

All 26 runs passed the oracle; the page cache check went from 100 % to 0 %
resident. Against the previous pass on this kind of host (2026-11-18, before
the sidecar removal; the host differs between sessions): Varlit's mixed-live
windows are no longer visible before they are durable (95.6 % then, 0 % now)
at the cost of more late windows (2.3 % then) and a doubled publish p99;
this comes from publishing stream data only once durable, which the macOS
A/B shows on both sides of the sidecar removal. oltp-sparse transfers took
48 % longer for Varlit and 37 % longer for SQLite; whether part of that is a
Varlit regression is open.

## Where Varlit is behind

- **grid:** reads 1.3x (Linux) to 1.7x (macOS) HDF5, writes 1.2x to 1.4x.
- **oltp cold point reads and aggregates:** SQLite is faster on both machines.
- **analytics range queries (Q2):** Parquet is about 25 % faster on Linux.
- **cells on Linux:** SQLite updates 1.4x faster with lower follower lag.
- **oltp-sparse load:** about 5x (Linux) to 10x HDF5's load time.
- **mixed-live lateness:** waiting for durability makes 5.4 % of windows late
  on Linux, against HDF5's 2.0 % (which shows undurable data).
- **mixed variance:** throughput swings by 3x between runs on macOS; the
  cause is open.
- **oltp-sparse transfers on Linux:** 48 % slower than the previous pass; no
  same-host A/B covers this scenario yet.

## Rerunning

```sh
cargo build --release --locked --manifest-path tools/format-bench/Cargo.toml
tools/format-bench/target/release/varlit-format-bench evict-check /abs/real-disk/dir
python3 tools/format-bench/run.py --data-gb 1 --repeats 3 --gate --out /abs/real-disk/dir
python3 tools/format-bench/report.py /abs/real-disk/dir
```

Add results for a new host under `tools/format-bench/results-<host>/` and a
row to this page.
