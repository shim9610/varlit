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

Two runs, one per machine. Each table lists, per metric, the value every
engine measured in each run. Compare engines within one run only: the two
machines differ in hardware, data size and sync cost, so a value from one
run says nothing about the other.

| Run | Machine | Data | Repeats | Commit | Full report |
| --- | --- | --- | --- | --- | --- |
| macOS 1 GB (2026-11-27) | Mac mini, Apple M4 (10 cores), 16 GiB, internal SSD, APFS | 1 GB per scenario (grid 2 GiB) | 3, median | `39edc54` | [results-mac/20261127.md](../tools/format-bench/results-mac/20261127.md) |
| Linux 10 GB (2026-11-27) | Cloud VM, Intel Xeon 2.10 GHz (4 vCPUs), 15.7 GiB, virtio disk, ext4 | 10 GB per scenario | 1 | `f290766` | [results-10gb-20261127.md](../tools/format-bench/results-10gb-20261127.md) |

Both runs use sidecar-free streams and `device_flush: full` (on macOS,
`F_FULLFSYNC` and SQLite `fullfsync`). Every run passed the oracle (78 on
macOS, 26 on Linux) and the page cache check went from 100 % to 0 % resident
on both. Neither run was gated: the Mac's disk probe (3.9 s per 1000 full
flushes) exceeds the gate. On macOS `grid` ran without a memory cap; on
Linux it ran under a 4 GiB cap. A `-` means the engine is not run in that
scenario.

### grid

6-D f32 array with a growing time axis; reads are cold.

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| All reads (s, lower is better) | macOS 1 GB | 2.16 | 2.85 | 1.28 | 2.32 |
| All reads (s, lower is better) | Linux 10 GB | 1.57 | 1.58 | 1.20 | 1.81 |
| Time series (ms, lower is better) | macOS 1 GB | 27 | 29 | 17 | 24 |
| Time series (ms, lower is better) | Linux 10 GB | 66 | 53 | 42 | 85 |
| 16 sub-cubes (ms, lower is better) | macOS 1 GB | 1486 | 1989 | 949 | 1596 |
| 16 sub-cubes (ms, lower is better) | Linux 10 GB | 793 | 802 | 670 | 827 |
| Ensemble means (ms, lower is better) | macOS 1 GB | 48 | 118 | 46 | 56 |
| Ensemble means (ms, lower is better) | Linux 10 GB | 43 | 41 | 39 | 65 |
| Write (s, lower is better) | macOS 1 GB | 4.4 | 13.5 | 3.1 | 4.5 |
| Write (s, lower is better) | Linux 10 GB | 23.1 | 39.0 | 19.9 | 36.8 |
| File size (MB) | macOS 1 GB | 2181 | 2471 | 2165 | 2161 |
| File size (MB) | Linux 10 GB | 9988 | 11352 | 9935 | 9996 |

### oltp

Accounts with dense ids; durable transfers, point reads, aggregates.

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| 1000 transfers (s, lower is better) | macOS 1 GB | 4.02 | 5.03 | 6.17 | 10.81 |
| 1000 transfers (s, lower is better) | Linux 10 GB | 0.46 | 1.25 | 2.31 | 5.60 |
| Transfer p99 (ms, lower is better) | macOS 1 GB | 5.07 | 43.26 | 11.59 | 14.99 |
| Transfer p99 (ms, lower is better) | Linux 10 GB | 1.09 | 21.78 | 8.46 | 8.95 |
| 20k cold point reads (s, lower is better) | macOS 1 GB | 2.07 | 1.67 | 2.72 | 10.77 |
| 20k cold point reads (s, lower is better) | Linux 10 GB | 2.30 | 1.93 | 6.52 | 128.03 |
| 20k warm point reads (s, lower is better) | macOS 1 GB | 0.040 | 0.047 | 0.774 | 5.749 |
| 20k warm point reads (s, lower is better) | Linux 10 GB | 0.105 | 0.101 | 1.663 | 110.523 |
| 200 branch sums (s, lower is better) | macOS 1 GB | 0.041 | 0.024 | 0.076 | 0.141 |
| 200 branch sums (s, lower is better) | Linux 10 GB | 0.093 | 0.084 | 0.245 | 1.104 |
| Load (s, lower is better) | macOS 1 GB | 3.0 | 9.4 | 0.9 | 2.1 |
| Load (s, lower is better) | Linux 10 GB | 39.9 | 180.4 | 28.8 | 40.1 |
| File size (MB) | macOS 1 GB | 1115 | 1136 | 1002 | 1166 |
| File size (MB) | Linux 10 GB | 10865 | 11685 | 10017 | 11628 |

### oltp-sparse

The same accounts with sparse 64-bit keys (Parquet not run).

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| 1000 transfers (s, lower is better) | macOS 1 GB | 5.63 | 4.78 | 8.06 | - |
| 1000 transfers (s, lower is better) | Linux 10 GB | 1.91 | 1.80 | 7.85 | - |
| Transfer p99 (ms, lower is better) | macOS 1 GB | 25.96 | 42.44 | 9.08 | - |
| Transfer p99 (ms, lower is better) | Linux 10 GB | 13.26 | 23.57 | 15.47 | - |
| 20k cold point reads (s, lower is better) | macOS 1 GB | 2.10 | 1.56 | 5.97 | - |
| 20k cold point reads (s, lower is better) | Linux 10 GB | 1.88 | 1.21 | 16.70 | - |
| 20k warm point reads (s, lower is better) | macOS 1 GB | 0.048 | 0.050 | 3.591 | - |
| 20k warm point reads (s, lower is better) | Linux 10 GB | 0.121 | 0.136 | 7.798 | - |
| 200 branch sums (s, lower is better) | macOS 1 GB | 0.049 | 0.026 | 0.077 | - |
| 200 branch sums (s, lower is better) | Linux 10 GB | 0.129 | 0.079 | 0.269 | - |
| Load (s, lower is better) | macOS 1 GB | 9.8 | 10.5 | 0.9 | - |
| Load (s, lower is better) | Linux 10 GB | 158.9 | 196.5 | 30.0 | - |
| File size (MB) | macOS 1 GB | 1377 | 1346 | 1156 | - |
| File size (MB) | Linux 10 GB | 13635 | 13758 | 11556 | - |

### analytics

Append-only 12-column table; cold queries.

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| Q1 group-by (s, lower is better) | macOS 1 GB | 0.256 | 3.798 | 0.293 | 0.269 |
| Q1 group-by (s, lower is better) | Linux 10 GB | 3.859 | 79.731 | 4.997 | 4.138 |
| Q2 20 range queries (s, lower is better) | macOS 1 GB | 0.038 | 0.005 | 0.038 | 0.034 |
| Q2 20 range queries (s, lower is better) | Linux 10 GB | 0.089 | 0.097 | 0.077 | 0.067 |
| Write (s, lower is better) | macOS 1 GB | 1.5 | 5.0 | 0.9 | 1.4 |
| Write (s, lower is better) | Linux 10 GB | 32.3 | 94.3 | 23.4 | 33.3 |
| File size (MB) | macOS 1 GB | 614 | 1142 | 627 | 622 |
| File size (MB) | Linux 10 GB | 6140 | 11427 | 6218 | 6108 |

### mixed

Three typed streams written as fast as possible, four live followers.

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| Throughput (multiple of real time, higher is better) | macOS 1 GB | 10.38 | 1.93 | 10.31 | 1.87 |
| Throughput (multiple of real time, higher is better) | Linux 10 GB | 4.58 | 1.86 | 4.12 | 1.08 |
| Publish p99 (ms, lower is better) | macOS 1 GB | 15.13 | 139.55 | 37.81 | 127.86 |
| Publish p99 (ms, lower is better) | Linux 10 GB | 78.60 | 165.02 | 88.36 | 258.54 |
| Inconsistent views seen by followers (should be 0) | macOS 1 GB | 0 | 0 | 4 | 0 |
| Inconsistent views seen by followers (should be 0) | Linux 10 GB | 0 | 0 | 58 | 0 |
| Late reader catch-up (s, lower is better) | macOS 1 GB | 0.49 | 1.08 | 0.52 | 1.61 |
| Late reader catch-up (s, lower is better) | Linux 10 GB | 12.96 | 13.99 | 15.18 | 40.34 |
| Write (s, lower is better) | macOS 1 GB | 0.9 | 5.0 | 0.9 | 5.1 |
| Write (s, lower is better) | Linux 10 GB | 21.0 | 51.8 | 23.4 | 88.8 |
| File size (MB) | macOS 1 GB | 997 | 1004 | 999 | 2007 |
| File size (MB) | Linux 10 GB | 9972 | 9988 | 9992 | 20083 |

### mixed-live

The same streams in real time, 10 ms freshness bound, every window durable.

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| Late windows (%, lower is better) | macOS 1 GB | 2.25 | 69.48 | 1.35 | 100.00 |
| Late windows (%, lower is better) | Linux 10 GB | 5.37 | 24.72 | 1.98 | 99.11 |
| B-only follower late (%, lower is better) | macOS 1 GB | 1.87 | 66.39 | 1.25 | 100.00 |
| B-only follower late (%, lower is better) | Linux 10 GB | 5.31 | 20.99 | 1.81 | 92.93 |
| Windows seen before durable (%, should be 0) | macOS 1 GB | 0.00 | 24.45 | 100.00 | 22.06 |
| Windows seen before durable (%, should be 0) | Linux 10 GB | 0.00 | 23.33 | 90.92 | 23.18 |
| Inconsistent views (should be 0) | macOS 1 GB | 0 | 0 | 4 | 0 |
| Inconsistent views (should be 0) | Linux 10 GB | 0 | 0 | 140 | 0 |
| Publish p99 (ms, lower is better) | macOS 1 GB | 7.99 | 41.40 | 11.15 | 57.17 |
| Publish p99 (ms, lower is better) | Linux 10 GB | 8.61 | 29.04 | 8.53 | 98.96 |
| File size (MB) | macOS 1 GB | 1007 | 1005 | 1008 | 2010 |
| File size (MB) | Linux 10 GB | 9972 | 9988 | 9992 | 20157 |

### cells

Fixed 1 KiB cells updated in place, 64 per atomic update, four followers (Parquet cannot update in place).

| Metric | Run | Varlit | SQLite | HDF5 | Parquet |
| --- | --- | --- | --- | --- | --- |
| Updates per second (higher is better) | macOS 1 GB | 203 | 141 | 145 | - |
| Updates per second (higher is better) | Linux 10 GB | 269 | 373 | 170 | - |
| Follower lag p99 (ms, lower is better) | macOS 1 GB | 4.56 | 7.97 | 0.43 | - |
| Follower lag p99 (ms, lower is better) | Linux 10 GB | 7.45 | 3.11 | 4.58 | - |
| Torn or corrupt reads (should be 0) | macOS 1 GB | 0 | 0 | 1777 | - |
| Torn or corrupt reads (should be 0) | Linux 10 GB | 0 | 0 | 3333 | - |
| Initial load (s, lower is better) | macOS 1 GB | 1.5 | 5.5 | 3.5 | - |
| Initial load (s, lower is better) | Linux 10 GB | 17.1 | 78.5 | 50.4 | - |
| File size (MB) | macOS 1 GB | 963 | 1259 | 956 | - |
| File size (MB) | Linux 10 GB | 10092 | 13369 | 10077 | - |


### Varlit between revisions

`ab.py` on the Mac compared the revision before the stream `.vks` sidecar
removal (`03678d7`) with the one after it (`39edc54`) on oltp, analytics,
mixed, mixed-live and cells, 3 alternating runs per side. No metric was
flagged (ranges overlapping, or medians within 5 %). Full table in
[results-mac/20261127.md](../tools/format-bench/results-mac/20261127.md).

| Metric | Before (median, range) | After (median, range) |
| --- | --- | --- |
| oltp 1000 transfers (s) | 4.06 (4.03 to 4.10) | 4.04 (3.88 to 4.08) |
| oltp transfer p99 (ms) | 5.05 (5.04 to 6.01) | 5.06 (5.02 to 5.22) |
| analytics Q1 (s) | 0.264 (0.262 to 0.284) | 0.263 (0.261 to 0.281) |
| mixed throughput (x real time) | 4.27 (3.23 to 10.36) | 8.41 (3.22 to 9.71) |
| mixed-live late windows (%) | 3.22 (0.31 to 6.77) | 2.37 (0.17 to 8.12) |
| mixed-live publish p99 (ms) | 9.23 (4.80 to 9.44) | 9.58 (4.86 to 9.72) |
| cells updates per second | 145 (133 to 220) | 207 (150 to 229) |

`oltp-sparse` and `grid` were not part of that comparison. On Linux, the
previous 10 GB pass ([results-10gb-20261118.md](../tools/format-bench/results-10gb-20261118.md),
before the sidecar removal, on a different host) measured oltp-sparse
1000 transfers at 1.29 s for Varlit and 1.31 s for SQLite, against 1.91 s and
1.80 s now; whether Varlit's part of that change is a regression is open.

## Where Varlit is behind

Every metric (file size aside) where another engine measured a better value
than Varlit in the tables above. Where that engine also failed a correctness
check in the same scenario, the failure is listed beside its value.

| Scenario | Metric | Run | Varlit | Best value |
| --- | --- | --- | --- | --- |
| grid | All reads (s) | macOS 1 GB | 2.16 | HDF5 1.28 |
| grid | All reads (s) | Linux 10 GB | 1.57 | HDF5 1.20 |
| grid | Time series (ms) | macOS 1 GB | 27 | HDF5 17 |
| grid | Time series (ms) | Linux 10 GB | 66 | HDF5 42 |
| grid | 16 sub-cubes (ms) | macOS 1 GB | 1486 | HDF5 949 |
| grid | 16 sub-cubes (ms) | Linux 10 GB | 793 | HDF5 670 |
| grid | Ensemble means (ms) | macOS 1 GB | 48 | HDF5 46 |
| grid | Ensemble means (ms) | Linux 10 GB | 43 | HDF5 39 |
| grid | Write (s) | macOS 1 GB | 4.4 | HDF5 3.1 |
| grid | Write (s) | Linux 10 GB | 23.1 | HDF5 19.9 |
| oltp | 20k cold point reads (s) | macOS 1 GB | 2.07 | SQLite 1.67 |
| oltp | 20k cold point reads (s) | Linux 10 GB | 2.30 | SQLite 1.93 |
| oltp | 20k warm point reads (s) | Linux 10 GB | 0.105 | SQLite 0.101 |
| oltp | 200 branch sums (s) | macOS 1 GB | 0.041 | SQLite 0.024 |
| oltp | 200 branch sums (s) | Linux 10 GB | 0.093 | SQLite 0.084 |
| oltp | Load (s) | macOS 1 GB | 3.0 | HDF5 0.9 |
| oltp | Load (s) | Linux 10 GB | 39.9 | HDF5 28.8 |
| oltp-sparse | 1000 transfers (s) | macOS 1 GB | 5.63 | SQLite 4.78 |
| oltp-sparse | 1000 transfers (s) | Linux 10 GB | 1.91 | SQLite 1.80 |
| oltp-sparse | Transfer p99 (ms) | macOS 1 GB | 25.96 | HDF5 9.08 |
| oltp-sparse | 20k cold point reads (s) | macOS 1 GB | 2.10 | SQLite 1.56 |
| oltp-sparse | 20k cold point reads (s) | Linux 10 GB | 1.88 | SQLite 1.21 |
| oltp-sparse | 200 branch sums (s) | macOS 1 GB | 0.049 | SQLite 0.026 |
| oltp-sparse | 200 branch sums (s) | Linux 10 GB | 0.129 | SQLite 0.079 |
| oltp-sparse | Load (s) | macOS 1 GB | 9.8 | HDF5 0.9 |
| oltp-sparse | Load (s) | Linux 10 GB | 158.9 | HDF5 30.0 |
| analytics | Q2 20 range queries (s) | macOS 1 GB | 0.038 | SQLite 0.005 |
| analytics | Q2 20 range queries (s) | Linux 10 GB | 0.089 | Parquet 0.067 |
| analytics | Write (s) | macOS 1 GB | 1.5 | HDF5 0.9 |
| analytics | Write (s) | Linux 10 GB | 32.3 | HDF5 23.4 |
| mixed-live | Late windows (%) | macOS 1 GB | 2.25 | HDF5 1.35; windows seen before durable: 100.00; inconsistent views: 4 |
| mixed-live | Late windows (%) | Linux 10 GB | 5.37 | HDF5 1.98; windows seen before durable: 90.92; inconsistent views: 140 |
| mixed-live | B-only follower late (%) | macOS 1 GB | 1.87 | HDF5 1.25; windows seen before durable: 100.00; inconsistent views: 4 |
| mixed-live | B-only follower late (%) | Linux 10 GB | 5.31 | HDF5 1.81; windows seen before durable: 90.92; inconsistent views: 140 |
| mixed-live | Publish p99 (ms) | Linux 10 GB | 8.61 | HDF5 8.53; windows seen before durable: 90.92; inconsistent views: 140 |
| cells | Updates per second | Linux 10 GB | 269 | SQLite 373 |
| cells | Follower lag p99 (ms) | macOS 1 GB | 4.56 | HDF5 0.43; torn or corrupt reads: 1777 |
| cells | Follower lag p99 (ms) | Linux 10 GB | 7.45 | SQLite 3.11 |

Varlit's `mixed` throughput on the Mac ranged from 3.2 to 10.4 times real
time between runs of the same revision; the cause is open.

## Rerunning

```sh
cargo build --release --locked --manifest-path tools/format-bench/Cargo.toml
tools/format-bench/target/release/varlit-format-bench evict-check /abs/real-disk/dir
python3 tools/format-bench/run.py --data-gb 1 --repeats 3 --gate --out /abs/real-disk/dir
python3 tools/format-bench/report.py /abs/real-disk/dir
```

Add results for a new host under `tools/format-bench/results-<host>/` and a
row to this page.
