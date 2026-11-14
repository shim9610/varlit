# Cross-format benchmark: Varlit vs SQLite vs HDF5 vs Parquet

Standalone diagnostic crate (outside the workspace). Each engine contributes
the situation it was designed for: chunked n-d arrays (HDF5, `grid`),
transactional point updates (SQLite, `oltp`), column analytics (Parquet,
`analytics`), high-rate heterogeneous streams followed by several readers
(Varlit, `mixed`) and in-place updates of a bounded cell grid with followers
(the Varlit matrix, `cells`). A scenario is defined by its data and the application's
requirements; every engine then solves it with the best choice it offers,
including the layout, sync policy and read path its own documentation
recommends. Every result is verified against an in-memory oracle (a mismatch
aborts the run).

```sh
cargo build --release --manifest-path tools/format-bench/Cargo.toml
python3 tools/format-bench/run.py --repeats 3 --gate --out /abs/real-disk/dir
python3 tools/format-bench/report.py /abs/real-disk/dir      # results tables
target/release/varlit-format-bench bench <scenario> <format> <fresh-dir>
```

Use a real disk, not tmpfs: durable publication (fsync) is part of the work.
`--gate` runs a SQLite transaction probe before and after every repeat and
redoes a repeat if the disk slowed down during it (consumer SSDs degrade after
tens of GB of writes; this happened twice while developing the harness).

Engines: SQLite (rusqlite `bundled`), HDF5 1.14 (`hdf5-metno` `static`, with
the Blosc-zstd filter), Parquet (arrow-rs 60), Varlit from this checkout. All
single-threaded.

## How results are measured

- **Write** includes durable completion (fsync / `sync()` / commit).
- **Reads** are steady state: queries on an open handle with the OS cache warm.
  Open time is excluded because handles are meant to stay open. Cold-cache and
  open numbers are still recorded in `results.json` (`cold_*`, `*_open_ms`).
- `MB` is allocated disk space for all files, sidecars included.
- `+trusted` opens every Varlit reader with
  `ReadLimits::MISSING.with_read_validation(ReadValidation::TRUSTED)`: no data CRC
  and no optional structure checks. SQLite, HDF5 and Parquet (as configured
  here) do not checksum data, so this is the like-for-like read row. Writes are
  identical with or without it.

## Scenarios and layouts

### grid (favors HDF5): 6-D chunked array larger than memory

Model output as climate and simulation codes write it: f32
`[time 64, ensemble 8, variable 4, level 16, lat 256, lon 512]` (17.2 GB),
written one time step at a time, growing the time axis (the file is not
sized up front). run.py runs this scenario in a cgroup with
`MemoryMax=4G` (page cache counts toward it; Linux harness only), so the data
is about four times the memory the engine can use. `FB_GRID=small` selects a
268 MB shape for smoke tests.

Every engine stores the same chunk shape `[1, 2, 1, 4, 32, 32]` (32 KiB, one
time step per chunk). Any fixed chunking makes some directions expensive, so
instead of chosen queries the reader measures every direction: 8 lines along
each of the 6 axes, 4 planes over each of the 15 axis pairs, 16 random
sub-cubes `[8, 4, 2, 4, 64, 64]` and 4 ensemble means of a lat x lon plane.
One pass after evicting the page cache.

| format | layout |
|---|---|
| varlit | matrix block, one chunk per cell; the chunk grid maps to the 2-D keyspace as scan = (time, ensemble, variable) chunk, ch = (level, lat, lon) chunk, so chunks adjacent along lon are one `read_matrix_run_bytes_into`. Time is the outer part of scan, so a new time step appends scan rows: `with_growing_matrix_dimension("cs", 1)` (one scan row, 16.8 MB, per growing chunk record; 1/2/4/16 rows read 2.17/2.19/2.22/2.20 s and buffer up to 780 MB). Only the first axis can grow |
| hdf5 | native 6-D chunked dataset with an unlimited time axis, extended (`H5Dset_extent`) before each step; every selection is one hyperslab `H5Dread` |
| sqlite | one BLOB per chunk keyed by its linear chunk id (rowid), so each run of adjacent chunks is one rowid range scan; 32 KiB pages (best of 4/16/32/64 KiB), WAL with `synchronous=NORMAL`, durable by a final checkpoint, mmap on read |
| parquet | one row per chunk, one row group per time step, one page per chunk, page index, `RowSelectionPolicy::Selectors` (the default mask policy decodes every page) |

The append-log `varlit-indexed` layout of the old 2-D grid is gone: 524,288
chunks exceed the finite key domain limit (4096 per block).

### oltp (favors SQLite): 1M accounts

Load 1M accounts (dense ids, random `branch` among 10,000). Then 1000 durable
transactions, each applying the net balance deltas of 10 random transfers
(~20 accounts). Then 20,000 point lookups and 200 per-branch SUM/COUNT.

| format | layout |
|---|---|
| varlit-append | append-native, LSM style. Base snapshot written once: 256 accounts per indexed record (finite key `g`, 11 KiB), plus a branch -> ids record. Each transaction appends **one** delta record (~350 B) to a native log with `commit_durable()`. Readers follow the log incrementally and keep folded deltas in memory |
| varlit-matrix-packed | update-in-place alternative: 256 accounts per 11 KiB matrix cell, read-modify-write of touched cells, `sync()` per transaction, `compact_matrix()` at the end; branch index packed 1024 ids per cell |
| sqlite | INTEGER PRIMARY KEY, index on branch built after load, `UPDATE ... SET balance = balance + ?`, WAL + FULL |
| hdf5 | one dataset per column, CSR branch index; point-selection read + write per transaction, flush + fsync (no transactional atomicity) |
| parquet | sorted base file with page index; one delta file per transaction (fsync + rename); reader replays deltas; branch index file |

### analytics (favors Parquet): wide table

4M rows x 12 columns. Q1 = `SUM(price*qty) WHERE qty > 50 GROUP BY category`
(3 of 12 columns). Q2 = 20 x `COUNT, SUM(price)` over narrow ranges of the
sorted `ts` column.

| format | layout |
|---|---|
| varlit | 128k-row column chunks as finite-keyed `(col, rg)` variable records, zstd `fast`; zone-map record per row group; dictionary codes for strings |
| parquet | zstd(1), 128k-row row groups, delta encoding for `ts`, byte-stream-split for floats, dictionary only for low-cardinality columns, row-group statistics pruning |
| hdf5 | one chunked dataset per column, Blosc-zstd(1) + shuffle, `ts` zone map |
| sqlite | `ts` as INTEGER PRIMARY KEY, normalized category table, covering index `(qty, category, price)` for Q1 |

### mixed (favors Varlit): heterogeneous typed streams with live followers

Three record types with exact field types: A `t:u64, a:[i8;10], f:[bool;3],
x:[u16;500]` at 100 kHz (~1 KB, 100 MB/s), B `t:u64, x:[f32;32], status:u16` at
10 kHz, C `t:u64, code:u32, msg:String` at 1 kHz. One producer thread per type
builds one block per 10 ms window and feeds a single writer through bounded
queues. Followers are separate processes: `FB_ALL_READERS` read every type and
check that each observation shows all types at the same boundary; one more
reads B only.

The application states requirements, not a sync schedule:

- **freshness** (`FB_FRESH_MS`): a follower must see every window within this
  time of its completion. 10 ms is a high-refresh consumer (display, control
  loop); 100 ms is a monitoring/logging consumer.
- **loss bound** (`FB_LOSS_MS`): how much of the most recent data a power loss
  may lose. 0 means everything a follower has seen is durable; 1000 means up
  to one second may be lost.

Each engine makes data visible every freshness interval and durable every
loss interval, with its cheapest mechanism for each:

| format | visible | durable |
|---|---|---|
| varlit (stream) | `sync()` (stream followers advance on the generation `sync` publishes; `flush()` is not visible to them, so visibility and durability cannot be separated) | same `sync()` |
| varlit-native | `commit()` (transaction marker, no fsync) | `commit_durable()`, single-barrier commit (`transaction_marker(explicit, single_barrier)`, one `sync_all`) |
| sqlite | WAL `COMMIT` with `synchronous=NORMAL` | `synchronous=FULL` for that transaction; WAL checkpoints only at durable commits once ~4 MB accumulated (`wal_autocheckpoint=0`) |
| hdf5 | SWMR `H5Fflush` | `H5Fflush` + fsync |
| parquet | one file per type closed with its footer, then a marker renamed into place | the same, plus fsync of every file and marker since the last durable point, then the directory |

Metrics: `realtime_factor` with producers flat out (`max`, 2 s of data);
follower lag p50/p99 and the share of windows seen later than the freshness
bound (`all_late_frac`) with real-time emission (`paced`, 1 s); reader CPU
per MB decoded. A cross-type inconsistency is an observation in which the
types do not end at the same window.

### cells (favors the Varlit matrix): in-place cell updates with live followers

The situation the matrix is designed for: a bounded grid of fixed-size cells
that is overwritten in place, published atomically, and followed by readers
that must never see a half-applied update. 262,144 cells of 1 KiB (256 MiB).
Each batch overwrites 64 random cells (never cell 0) with a new version and
then cell 0 with the batch number and a timestamp; 2000 batches. Four
follower processes poll; when cell 0 shows a new batch B, the follower reads
all 64 cells of batch B (each must be at version B) and 64 random cells (each
must be at a version no newer than B and belong to that version's batch).
Each cell carries a full deterministic payload, so a cell whose bytes do not
match any whole version is counted as **corrupt**; an observation with a
wrong version or a corrupt cell is **torn**. Loss bound (`FB_LOSS_MS`) as in
`mixed`: 0 makes every batch durable, 1000 makes one durable per second.

| format | update | visible | durable |
|---|---|---|---|
| varlit-matrix | `matrix Cell { v: [u8; 1024] }`, 1024 x 256 keyspace, write the cell in place | `sync()` (the only publication; always durable) | same `sync()` |
| varlit-native | append `CellRec { id: u32, data: Vec<u8> }` per changed cell; followers keep an id -> record index updated by walking `block_chain` from the tail | `commit()` (transaction marker) | `commit_durable()` (single barrier) |
| sqlite | `INSERT OR REPLACE` of a BLOB row per cell in one transaction; readers wrap each poll in a read transaction | WAL `COMMIT`, `synchronous=NORMAL` | `synchronous=FULL`; checkpoint at durable commits past ~4 MB of WAL |
| hdf5 | (cells, 1024) u8 dataset chunked 16 x 1024, `write_slice` per cell, SWMR | `H5Fflush` | flush + fsync |
| parquet | no in-place update: **FAIL** | | |

## Results

2026-11-11, Linux 7.0, 24 cores, NVMe ext4, rustc 1.98.1, release build, branch
`perf/bulk-primitive-codec` (Varlit 0.13.0 plus the bulk codec, single-barrier
commit and caller-buffer API). Disk-gated (SQLite control 0.50 to 0.51 s). All
runs verified every record.

Single scenarios: medians of 3 gated repeats.

### Summary

Each engine is strong in its own scenario and each has clear weak spots.

| scenario | best | Varlit position |
|---|---|---|
| grid (HDF5) | Varlit and HDF5 level (all reads 2.18 vs 2.23 s); HDF5 writes fastest (16.9 s) | fastest on time series (70 vs 103 ms); only the first axis can grow (HDF5 can extend any axis); no mmap path at this size |
| oltp (SQLite) | SQLite for throughput (0.51 s / 1000 txn), point reads and aggregates | fastest load (0.06 s) and lowest p99 (1.18 ms), but 1.8x SQLite's transaction time |
| analytics (Parquet) | Varlit for Q1 group-by (47.6 ms); SQLite with its index for Q2 ranges (24.6 ms) | slowest on Q2 (38.5 ms vs Parquet 30.0); SQLite pays 4.7 s writes and 2.4x size |
| mixed (Varlit) | varlit-native for throughput in every cell, the only format with 0 % late windows | stream Varlit cannot use a loss bound and degrades with four followers |
| cells (Varlit matrix) | varlit-native (1522 / 6440 batches/s), then SQLite; no torn reads for either | the matrix is the slowest updater (483 batches/s at any loss bound) and grows 1.7 GB for 133 MB of updates; HDF5 readers saw torn and corrupt cells |

Late % is the share of windows a follower saw after the freshness bound in
real-time emission; 0 % means every window met it. Varlit loses clearly on
oltp throughput and analytics range reads.

### grid: 6-D f32 array larger than memory, growing time axis (HDF5-favored)

17.2 GB under a 4 GiB memory cap (page cache included); one pass from a cold
cache, medians of 3 gated repeats. Bold = best per row.

| selection | varlit | varlit+trusted | sqlite | hdf5 | parquet |
|---|---|---|---|---|---|
| write s | 18.8 | 17.6 | 37.4 | **16.9** | 30.1 |
| GB | 17.18 | 17.18 | 19.63 | 17.18 | 17.29 |
| peak RSS MB | 326 | 326 | **265** | 328 | 1418 |
| all reads ms | 2175.1 | **2152.2** | 3757.5 | 2233.9 | 3430.2 |
| 8 lines along time ms | 69.9 | **63.6** | 143.8 | 103.0 | 146.0 |
| 8 lines along ens ms | 4.6 | **4.1** | 8.4 | 5.2 | 7.7 |
| 8 lines along var ms | 4.3 | **4.0** | 8.8 | 4.3 | 7.6 |
| 8 lines along level ms | 3.9 | **3.8** | 8.6 | 4.2 | 7.9 |
| 8 lines along lat ms | **7.8** | 7.9 | 17.7 | 8.2 | 15.6 |
| 8 lines along lon ms | 6.1 | **6.1** | 7.7 | 6.3 | 7.2 |
| 4 planes time x ens ms | 133.6 | **123.8** | 274.8 | 141.2 | 257.7 |
| 4 planes time x var ms | 127.5 | **126.2** | 273.2 | 128.8 | 256.6 |
| 4 planes time x level ms | **125.1** | 126.2 | 261.8 | 130.4 | 242.9 |
| 4 planes time x lat ms | 245.5 | **245.4** | 544.1 | 258.4 | 505.1 |
| 4 planes time x lon ms | 193.9 | **187.5** | 251.3 | 203.4 | 238.6 |
| 4 planes ens x var ms | 7.5 | **7.4** | 16.6 | 8.4 | 15.9 |
| 4 planes ens x level ms | 8.0 | **7.6** | 16.2 | 8.4 | 15.2 |
| 4 planes ens x lat ms | 15.5 | **15.2** | 32.8 | 15.7 | 31.5 |
| 4 planes ens x lon ms | **11.2** | 11.4 | 15.2 | 12.2 | 13.0 |
| 4 planes var x level ms | **7.4** | 7.6 | 16.0 | 8.0 | 15.1 |
| 4 planes var x lat ms | 15.8 | **15.2** | 32.3 | 15.9 | 29.1 |
| 4 planes var x lon ms | 11.1 | 11.3 | 15.0 | **10.8** | 13.4 |
| 4 planes level x lat ms | 15.9 | 16.5 | 34.9 | **15.8** | 28.4 |
| 4 planes level x lon ms | 11.6 | **11.5** | 16.0 | 12.1 | 15.0 |
| 4 planes lat x lon ms | **8.1** | 8.2 | 9.7 | 8.6 | 10.4 |
| 16 cubes [8,4,2,4,64,64] ms | 1110.1 | 1104.1 | 1702.8 | **1092.2** | 1513.1 |
| 4 ensemble means ms | 34.8 | **33.2** | 38.0 | 37.4 | 43.9 |

- Varlit and HDF5 are level overall (2.18 vs 2.23 s); Varlit is faster on
  time series (70 vs 103 ms) and ensemble means, HDF5 on writes and cubes by a
  few percent. SQLite and Parquet take 1.5-1.7x as long.
- Each engine reads one 32 KiB chunk (or one run of adjacent chunks) per
  positional read. Before VMAT v6 and the coalesced generation read, Varlit
  split every cell into three reads; the fixed-extent layout measured 3.44 s
  then, 2.65 s with v6, and the growing layout (time steps after the first are
  append-log chunk records, read directly) 2.18 s.
- Whole-file `mmap_matrix()` cannot serve this size: it materializes the
  matrix's copy-on-write pages and is refused by the mmap-length limit
  (`LimitExceeded`, 8 GiB). `mmap_matrix_cell` does not cover growing chunks
  (`MatrixKeyOutOfBounds`), so no mmap row is listed.

### oltp: 1M accounts, durable transactions (SQLite-favored)

| format | load s | 1000 txn s | txn p50 ms | txn p99 ms | 20k points ms | 200 branch aggregates ms | MB |
|---|---|---|---|---|---|---|---|
| varlit-append | 0.06 | 0.94 | 0.92 | 1.18 | 57.5 | 55.3 | 55 |
| varlit-append+trusted | 0.06 | 0.97 | 0.95 | 1.22 | 35.6 | 33.7 | 55 |
| varlit-matrix-packed+trusted | 0.15 | 1.59 | 1.54 | 2.10 | 57.5 | 57.8 | 53 |
| sqlite | 0.61 | 0.51 | 0.32 | 3.69 | 26.7 | 8.1 | 56 |
| hdf5 | 0.06 | 0.82 | 0.82 | 1.26 | 472.9 | 9.2 | 52 |
| parquet | 0.11 | 1.10 | 1.06 | 1.39 | 550.1 | 52.6 | 65 |


### analytics: 4M rows x 12 columns (Parquet-favored)

| format | write s | Q1 group-by ms | Q2 20 ranges ms | MB |
|---|---|---|---|---|
| varlit | 0.56 | 47.6 | 38.5 | 173 |
| varlit+trusted | 0.56 | 47.5 | 38.0 | 173 |
| sqlite | 4.72 | 234.3 | 24.6 | 421 |
| hdf5 | 0.37 | 53.1 | 30.8 | 175 |
| parquet | 0.60 | 54.5 | 30.0 | 172 |

### mixed: heterogeneous typed streams (`sweep_mixed.py`, medians of 2 gated repeats)

Columns are freshness / loss bound. Throughput = multiple of real time the
writer sustains with producers flat out (1.0x = 101 MB/s of input), while every
follower verifies every record. Lag = window completion to verification by
the slowest all-types follower, in real-time emission; late % = windows seen
after the freshness bound. Reader MB/CPU-s from the 10 ms / 1 s throughput run.
Inconsistent = observations in which the types did not end at the same window,
summed over all runs of that row.

One all-types follower (plus a B-only follower):

| format | 10 ms / 0 | 10 ms / 1 s | 100 ms / 0 | 100 ms / 1 s | lag p50/p99 ms, 10 ms / 0 | late %, 10 ms / 0 · 100 ms / 0 | reader MB/CPU-s | inconsistent |
|---|---|---|---|---|---|---|---|---|
| varlit | 5.9x | 6.0x | 13.3x | 13.3x | 2.0 / 4.4 | 0.0 · 4.5 | 1752 | 0 |
| varlit-native | 8.3x | 16.9x | 14.1x | 16.7x | 1.2 / 2.1 | 0.0 · 0.0 | 2496 | 0 |
| sqlite | 3.6x | 5.2x | 4.7x | 5.3x | 2.7 / 4.8 | 0.0 · 9.0 | 1251 | 0 |
| hdf5 | 7.4x | 14.8x | 12.2x | 14.7x | 1.8 / 3.0 | 0.0 · 6.3 | 1461 | 1 |
| parquet | 1.4x | 1.8x | 3.2x | 2.9x | 8.3 / 13.8 | 6.7 · 53.7 | 537 | 0 |

Four all-types followers (plus a B-only follower):

| format | 10 ms / 0 | 10 ms / 1 s | 100 ms / 0 | 100 ms / 1 s | lag p50/p99 ms, 10 ms / 0 | late %, 10 ms / 0 · 100 ms / 0 | reader MB/CPU-s | inconsistent |
|---|---|---|---|---|---|---|---|---|
| varlit | 3.7x | 5.9x | 12.2x | 9.5x | 2.2 / 6.1 | 0.0 · 5.9 | 1667 | 0 |
| varlit-native | 8.3x | 15.6x | 13.6x | 13.4x | 1.2 / 2.4 | 0.0 · 0.0 | 2335 | 0 |
| sqlite | 3.6x | 5.2x | 4.7x | 4.4x | 2.8 / 4.8 | 0.0 · 8.8 | 1446 | 0 |
| hdf5 | 6.6x | 13.1x | 10.8x | 10.6x | 1.9 / 3.5 | 0.0 · 6.8 | 1287 | 6 |
| parquet | 1.4x | 1.6x | 2.1x | 2.4x | 9.4 / 14.3 | 16.0 · 60.2 | 404 | 0 |

- varlit-native is the fastest writer in every cell and the only format with
  no late windows. Relaxing the loss bound to 1 s doubles it (8.3x -> 16.9x),
  because `commit()` publishes without a barrier.
- varlit (stream) cannot separate visibility from durability: it pays a
  `sync()` per freshness interval whatever the loss bound, and its throughput
  drops with four followers at 10 ms (5.9x -> 3.7x).
- HDF5 SWMR flushes are not atomic across datasets: followers occasionally
  saw types ending at different windows (up to 3 per run group), which the
  per-window t0 marker does not prevent.
- Parquet must close a file per type per publication; it misses the 10 ms
  bound in real time and is far behind at every setting.

### cells: in-place cell updates with four followers (`sweep_cells.py`, medians of 3 gated repeats)

2026-11-12, same host, disk-gated (SQLite control 0.49 to 0.50 s, after
`fstrim`). 2000 batches of 64 + 1 cells. Lag = cell 0 publication to a
follower seeing it (worst follower). Torn and corrupt are summed over the
three repeats. Growth = file growth during the 2000 batches (logical updates:
2000 x 65 KiB = 133 MB).

| format | batches/s, loss 0 | batches/s, loss 1 s | publish p50/p99 ms, loss 0 | lag p50/p99 ms, loss 0 | lag p50/p99 ms, loss 1 s | torn (obs) | corrupt | growth |
|---|---|---|---|---|---|---|---|---|
| varlit-matrix | 483 | 481 | 1.56 / 3.46 | 1.25 / 2.03 | 1.25 / 2.08 | 0 | 0 | 1716 MB |
| varlit-native | 1522 | 6440 | 0.51 / 0.72 | 0.27 / 0.39 | 3.29 / 6.44 | 0 | 0 | 147 MB |
| sqlite | 1059 | 2509 | 0.64 / 12.15 | 0.73 / 1.21 | 0.22 / 0.40 | 0 | 0 | WAL truncated at checkpoint |
| hdf5 | 614 | 1672 | 1.07 / 6.42 | 0.11 / 0.64 | 0.29 / 0.63 | 27 (of 7991) · 407 (of 4241) | 4 | 0 (in place) |
| parquet | FAIL | FAIL | | | | | | |

- The matrix does not win its own scenario. It is the slowest updater, and
  relaxing the loss bound changes nothing, because `sync()` is its only
  publication and always durable.
- Each publication adds about 860 KB to the file for 65 KiB of changed cells:
  copy-on-write rewrites the whole 4 KiB page of every changed 1 KiB cell, plus
  page-directory pages. The file grew by the same amount every batch (1.7 GB
  after 2000 batches); this run did not call `compact_matrix()`.
- varlit-native appends only the changed cells (~74 KB per batch) and is the
  fastest at both loss bounds. Its followers pay for an id -> record index
  (lag p99 6.4 ms at a 1 s loss bound, when the writer runs far ahead).
- SQLite is atomic and compact; its p99 publish time comes from checkpoints.
- HDF5 SWMR has no multi-write atomicity: followers saw batches partly applied
  (torn) and, with a 0 loss bound, cells whose 1 KiB payload was itself
  half-written (corrupt, 4 cells). Before corrupt reads were counted, two
  sweep runs aborted on them.
- Parquet files cannot be updated in place.

## Contracts are not identical

- Integrity: Varlit stores CRC32 over header+payload (verified unless `+trusted`);
  the others store no data checksums here.
- Publication: SQLite WAL and Varlit transaction markers commit atomically.
  Varlit stream/indexed `sync()` also publishes a crash-recoverable sidecar
  generation (two fsync-class calls per publication). HDF5 flush + fsync is
  not transactional across datasets. Parquet relies on rename.
- Loss bound: with a non-zero bound, SQLite (`synchronous=NORMAL`), Varlit
  (`commit()`), HDF5 (flush without fsync) and Parquet (no fsync) may all lose
  the not-yet-durable publications on power loss; each still recovers to a
  consistent earlier state by its own mechanism (WAL frame checksums, marker
  checksum chain, none for HDF5, footer validation for Parquet).

## Appendix: configurations that were tried and replaced

Measured during development on the same host (separate runs; not part of the
gated final run). They document usage mistakes and their costs.

| what | observation | replaced by |
|---|---|---|
| live follower calling `block_entries_into` every poll | re-lists the whole directory each poll: 476 ms per follower, lag p50 2.6 ms | `block_chain` walk from the tail: lag p50 0.7 ms |
| oltp: one indexed record per account (1M generic B-tree keys) | load 4.2 s; `.vki` grows to 1.9 GB during load and +180 KB per transaction until `compact_index()` (2.3 GB -> 212 MB) | grouped base + delta log |
| oltp: one matrix cell per account | load 4.7 s (2M cell write+commit calls); 61 MB | 256 accounts per cell |
| oltp: grouped records rewritten per transaction (RMW) | group 256 / 1024 / 4096 / 16384 accounts: 1000 txn 2.5 / 2.9 / 11.0 / 22.2 s, 281 MB to 12.5 GB (old versions stay in the log) | delta log (one small record per transaction) |
| grid (earlier 2-D version): `compact_matrix()` before reads | matrix reads are 4 KiB `pread`s of generation pages either way; cold columns 0.17 -> 0.15 s | indexed tiles or mmap for read-heavy grids |
| parquet default row selection | mask policy decoded every page: 16 columns 1.38 s | `Selectors`: 0.01 s |
| parquet default encodings | 216 MB, write 1.19 s | per-column encodings: 172 MB, 0.59 s |
| hdf5 shuffle + deflate(1) | write 3.1 s, Q1 0.14 s | Blosc-zstd(1): 0.37 s, 0.05 s |
| sqlite 32 KiB pages for 16 KiB tiles (earlier 2-D version) | one tile per page, 537 MB | default 4 KiB pages: 277 MB |
| grid sqlite page size for 32 KiB chunks (268 MB smoke shape) | 4 / 16 / 32 / 64 KiB pages: 272 / 285 / 306 / 537 MB; 32 cubes 12.8 / 12.9 / 10.9 / 13.9 ms | 32 KiB pages |
