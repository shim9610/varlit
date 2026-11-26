# Varlit cross-format benchmark on macOS

Run on November 26, 2026 (KST).

## Hardware, software, and test conditions

| Item | Value |
| --- | --- |
| Mac | Mac mini (Mac16,10), Apple M4, 10 CPU cores (4 performance, 6 efficiency), 16 GiB RAM |
| OS and architecture | macOS 27.2 (26B123), native arm64, Rosetta translation off |
| Storage | Internal Apple solid-state drive, 245.1 GB reported by diskutil, Apple Fabric, local journaled APFS on the Data volume |
| Free space | 40 GiB before cleanup and 52 GiB after cleanup (df); 17.7 GiB of regenerable Loom Cargo outputs removed by du |
| Power | AC power, Low Power Mode off, serial execution with caffeinate -i during timing |
| Toolchain | rustc 1.97.1 (8bab26f4f 2026-07-14), Cargo 1.97.1, CMake 4.5.1, Python 3.9.6 |
| Varlit source | main at `03678d78ab3d74e94a3ed721dbef01b5f339aed3`, 0.13.0-alpha.2 |
| Build | Release, locked dependencies, default features with bundled HDF5, native arm64 |
| Workload and gate | 1 GB logical data per scenario, three repeats, disk gate ungated |

The disk control is 1,000 iterations of a 4 KiB positional write followed by Rust `File::sync_data`. The supplied 0.7 s gate was omitted after each of the three preflight controls exceeded the limit.

Preflight control values were 3.928119 s, 3.924149 s, 3.922831 s. The post-run control was 4.045381 s.

## Cross-format results

The supplied runner recorded 66 processes across six scenarios. 66 completed successfully and 0 failed. The tables below are the unedited output of `report.py`.

### oltp: accounts with dense ids, durable transfers (SQLite's case) (1 GB)

| engine | 1000 txn s | txn p99 ms | 20k cold points s | 20k warm points s | 200 branch sums s | load s | MB |
|---|---|---|---|---|---|---|---|
| varlit | 4.31 | 5.23 | 0.09 | 0.041 | 0.042 | 3.0 | 1114 |
| sqlite | 0.44 | 16.84 | 0.05 | 0.051 | 0.026 | 9.3 | 1138 |
| hdf5 | 4.92 | 10.97 | 0.83 | 0.815 | 0.076 | 0.9 | 1002 |
| parquet | 9.81 | 13.97 | 5.62 | 5.623 | 0.137 | 1.6 | 1166 |

### oltp-sparse: the same accounts with sparse 64-bit keys (1 GB)

| engine | 1000 txn s | txn p99 ms | 20k cold points s | 20k warm points s | 200 branch sums s | load s | MB |
|---|---|---|---|---|---|---|---|
| varlit | 5.64 | 25.04 | 0.08 | 0.053 | 0.054 | 10.3 | 1367 |
| sqlite | 0.52 | 17.73 | 0.05 | 0.052 | 0.028 | 10.3 | 1347 |
| hdf5 | 7.97 | 9.14 | 3.68 | 3.660 | 0.079 | 0.9 | 1156 |

### analytics: append-only 12-column table, cold queries (Parquet's case) (1 GB)

| engine | Q1 group-by s | Q2 20 ranges s | write s | MB |
|---|---|---|---|---|
| varlit | 0.193 | 0.034 | 1.5 | 625 |
| parquet | 0.163 | 0.024 | 1.4 | 617 |
| sqlite | 3.466 | 0.005 | 4.9 | 1141 |
| hdf5 | 0.218 | 0.034 | 0.9 | 622 |

### mixed: three typed streams flat out, four live followers (Varlit's case) (1 GB)

| engine | x real time | publish p99 ms | inconsistent views | late reader catch-up s | write s | MB |
|---|---|---|---|---|---|---|
| varlit | 10.26 | 14.73 | 0 | 0.22 | 0.9 | 997 |
| sqlite | 4.06 | 27.35 | 0 | 0.24 | 2.4 | 1003 |
| hdf5 | 11.23 | 15.66 | 5 | 0.28 | 0.9 | 999 |
| parquet | 1.95 | 136.07 | 0 | 0.99 | 4.9 | 2007 |

### mixed-live: the same in real time, 10 ms freshness, every window durable (1 GB)

| engine | late windows % | B-only follower late % | seen before durable % | inconsistent views | publish p99 ms | MB |
|---|---|---|---|---|---|---|
| varlit | 7.47 | 7.39 | 0.00 | 0 | 10.72 | 1007 |
| sqlite | 0.17 | 0.10 | 19.98 | 0 | 6.80 | 1005 |
| hdf5 | 0.00 | 0.00 | 100.00 | 4 | 9.89 | 1009 |
| parquet | 100.00 | 100.00 | 65.22 | 0 | 41.81 | 2014 |

### cells: fixed 1 KiB cells updated in place, 64 per atomic update, four followers (1 GB)

| engine | updates/s | follower lag p99 ms | torn + corrupt reads | growth MB | initial load s | MB |
|---|---|---|---|---|---|---|
| varlit | 173 | 5.30 | 0 | -0 | 1.6 | 963 |
| sqlite | 913 | 0.93 | 0 | -1 | 2.8 | 1265 |
| hdf5 | 146 | 0.54 | 2497 | 0 | 3.7 | 956 |

## Compatibility, failures, and exclusions

- The unmodified harness failed to link on Darwin because it declared Linux `posix_fadvise` for all Unix targets. The local harness patch uses Darwin’s monotonic and process CPU clock IDs, and keeps file synchronization while skipping the page-cache eviction call on macOS. This patch applies only to the benchmark harness. The `cold` columns label the harness’s first read and can include OS-cached data.
- The harness's process peak RSS helper reads Linux `/proc/self/status`. A zero or absent RSS field on macOS is unavailable as a memory measurement.
- `grid` was omitted because its runner requires a Linux systemd or cgroup memory limit. An unrestricted grid run was outside this campaign.
- HDF5 was included. The run plan omits Parquet from sparse OLTP and fixed-cell updates. Those combinations were not scheduled.
- No process failure or oracle rejection appeared in the cross-format result records. The supplied tables retain any scenario-level consistency and corruption counters.

Nonzero consistency counters from successful processes are listed by repeat.

- mixed / hdf5, `all_inconsistent` — repeat 1: 8, repeat 2: 4, repeat 3: 5.
- mixed-live / hdf5, `all_inconsistent` — repeat 1: 4, repeat 2: 6, repeat 3: 4.
- cells / hdf5, `torn` — repeat 1: 2497, repeat 2: 2608, repeat 3: 2420.
- The HDF5 cells adapter documents that SWMR readers can observe part of a multi-cell update. A torn observation and a corrupt payload are separate counters.

## Same-Mac Varlit A/B

Base `ccf93a2981fcbbd995f31d71bad59c702cb1714c` and head `03678d78ab3d74e94a3ed721dbef01b5f339aed3`, 1 GB, three alternating repeats per side. Harness selection was **current harness on base and head**.
The base uses its own lockfile, whose only differences from head are the three local Varlit package versions. Each binary and its SHA-256 are retained separately.

## Varlit A/B on Darwin arm64

ccf93a2 vs 03678d7, 3 alternating runs per side, 1 GB per scenario. Median (min to max); change is head vs base. Flagged only when the ranges do not overlap and the medians differ by 5 % or more.

### oltp: accounts with dense ids, durable transfers (SQLite's case)

| metric | base | head | change | |
|---|---|---|---|---|
| 1000 txn s | 4.21 (3.63 to 4.28) | 4.19 (4.01 to 4.35) | -0.4% |  |
| txn p99 ms | 5.17 (5.10 to 6.09) | 5.32 (5.16 to 5.55) | +3.0% |  |
| 20k cold points s | 0.09 (0.09 to 0.10) | 0.09 (0.09 to 0.09) | -0.9% |  |
| 20k warm points s | 0.043 (0.042 to 0.046) | 0.042 (0.041 to 0.043) | -1.4% |  |
| 200 branch sums s | 0.044 (0.043 to 0.045) | 0.044 (0.044 to 0.044) | +0.2% |  |
| load s | 2.9 (2.9 to 3.0) | 2.9 (2.9 to 2.9) | -1.2% |  |
| MB | 1112 (1107 to 1114) | 1115 (1111 to 1116) | +0.2% |  |

### analytics: append-only 12-column table, cold queries (Parquet's case)

| metric | base | head | change | |
|---|---|---|---|---|
| Q1 group-by s | 0.194 (0.193 to 0.194) | 0.195 (0.194 to 0.195) | +0.5% |  |
| Q2 20 ranges s | 0.034 (0.034 to 0.035) | 0.034 (0.034 to 0.034) | +0.0% |  |
| write s | 1.5 (1.5 to 1.5) | 1.5 (1.5 to 1.5) | +0.2% |  |
| MB | 626 (620 to 626) | 625 (625 to 626) | -0.1% |  |

### mixed: three typed streams flat out, four live followers (Varlit's case)

| metric | base | head | change | |
|---|---|---|---|---|
| x real time | 10.68 (10.06 to 12.49) | 8.10 (7.92 to 10.23) | -24.1% |  |
| publish p99 ms | 37.50 (12.01 to 43.03) | 35.84 (14.22 to 37.46) | -4.4% |  |
| inconsistent views | 0 (0 to 0) | 0 (0 to 0) | +0.0% |  |
| late reader catch-up s | 0.22 (0.22 to 0.22) | 0.22 (0.22 to 0.22) | -0.1% |  |
| write s | 0.9 (0.8 to 1.0) | 1.2 (0.9 to 1.2) | +31.8% |  |
| MB | 997 (997 to 997) | 998 (997 to 998) | +0.1% |  |

### cells: fixed 1 KiB cells updated in place, 64 per atomic update, four followers

| metric | base | head | change | |
|---|---|---|---|---|
| updates/s | 187 (141 to 208) | 150 (143 to 178) | -20.0% |  |
| follower lag p99 ms | 4.51 (4.12 to 4.71) | 4.49 (4.44 to 4.89) | -0.5% |  |
| growth MB | -0 (-0 to -0) | -0 (-0 to -0) | -0.0% |  |
| initial load s | 1.5 (1.4 to 1.6) | 1.5 (1.5 to 1.7) | +1.9% |  |
| MB | 963 (963 to 963) | 963 (963 to 974) | +0.0% |  |

The supplied rule flagged no metric. Mixed throughput and cell update throughput had lower head medians by 24.1% and 20.0%, respectively, with overlapping observed ranges. These are unresolved differences under the three-run rule.

## Durability and comparability

On macOS, plain `fsync(2)` sends dirty data from the host to the device. `F_FULLFSYNC` also asks the device to flush its write cache. The Rust toolchain used here implements both Apple `File::sync_data` and `File::sync_all` with `F_FULLFSYNC`, including the disk control. Engine adapters can take different synchronization paths. The benchmark checks logical data against its oracles and records its durability-related counters. Power-loss recovery was outside this run. [Apple fsync documentation](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/fsync.2.html), [Rust 1.97.1 implementation](https://github.com/rust-lang/rust/blob/8bab26f4f68e0e26f0bb7960be334d5b520ea452/library/std/src/sys/fs/unix.rs#L1303-L1322).

The SQLite adapters use `synchronous=FULL` for their durable transaction or batch paths and leave SQLite’s separate `fullfsync` setting at its default (off). The SQLite and Rust-backed timings therefore reflect each adapter’s configured durability path. The cost of this setting difference was not measured separately. [SQLite fullfsync documentation](https://sqlite.org/pragma.html#pragma_fullfsync).

Read this report as a Mac baseline. The archived 10 GB Linux VM campaign used different hardware, a different filesystem and synchronization stack, a larger working set, the older Varlit revision, and a different cold-cache regime. Row-by-row ratios against it cannot isolate the code changes. The paired Mac A/B provides the within-machine comparison.

## Reproduction and local evidence

Apply the retained `macos-harness.patch` from the checkout root before building. Keep the run directory on APFS. Commands below were used for the cross-format run.

```sh
cargo build --offline --release --locked --manifest-path tools/format-bench/Cargo.toml
cd tools/format-bench
caffeinate -i python3 -u run.py --data-gb 1 --repeats 3 --only oltp,oltp-sparse,analytics,mixed,mixed-live,cells --out "$PWD/target/runs-mac"
python3 report.py "$PWD/target/runs-mac"
```

Raw cross-format data and the controls are in `tools/format-bench/target/runs-mac/`. Raw A/B data, if completed, is in `ab-mac/` with the original A/B table in `ab-mac.md`. The enclosing artifact directory retains environment, patch, build logs, binary hashes, and the console logs. The stock runners remove temporary per-process databases after each process.
