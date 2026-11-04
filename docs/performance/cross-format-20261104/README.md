# Performance comparison with other binary formats — 2026-11-04

The measured PR head is `291a6aa79fbb888a3d74e61e013395a77c66a5a1`; its production Rust matches verified runtime `1e59239`. Varlit native, stream, and indexed paths were compared with HDF5 1.14.5, SQLite 3.46.1, and a raw-file baseline. **35 conditions × three repetitions = 105 observations**, plus ten separate resource probes. The final selected comparisons have no execution failures or data mismatches.

## Scope correction

This report contains auxiliary single-payload and small-frame diagnostics. **It does not represent Varlit's primary use case: mixed declared blocks and metadata, concurrent readers following newly committed records, and high-cardinality key lookup.** Any inference about whole-library practicality or satisfaction of the 2× SQLite target from these measurements alone is withdrawn. Measurements and all failure records remain preserved below.

In particular, the 128 B × 131,072 condition stores every item as a separate Varlit frame. It does not measure ordinary logging that groups 131,072 logical samples into arrays or chunks. Half that condition's SQLite time, 15.3 ms from 30.5 ms, is not the acceptance target for ordinary grouped sample logging.

Physical frame counts and logical item counts differ. For example, grouping 131,072 logical 128 B items (16 MiB) into 64 KiB payloads would produce 256 data frames; 1 MiB groups would produce 16. This is an illustrative calculation, not a measured logical-array workload. In contrast, 131,072 independent high-cardinality metadata objects may each be actual records. These cases have different meanings.

The primary evaluation and comparison schemas are specified in the [mixed-append comparison plan](../mixed-append-comparison-plan-20261104.md). Results from executing that plan are not included in this report.

The following diagnostics require careful interpretation.

- **Individual small frames:** sequential decoding and file space for 128 B × 131,072 frames are worse than the SQLite schema used here. Preserve the observation without treating it as a practicality verdict for ordinary grouped samples.
- **Native record-index preparation:** one-time preparation grows with record count. Separate it from reads through a persistent handle. Stream/indexed paths provide different fast-open behavior.
- **Eight readers each consuming everything:** particularly for 4 GiB / 4 MiB, per-reader throughput under four CPUs, including full-byte validation, is below the payload target for 64-bit 100 MS/s. Other group sizes and paths are shown separately. This is not a reproduced concurrent-ingestion failure or an unlimited real-time consumption guarantee.

The large indexed batch regression in the earlier full performance report includes **updates to existing keys and deletions in the second half**. This benchmark appends unique keys/rows. In `scalable_load.rs`, `key_at` reuses keys after halfway and `finish_epoch` deletes some keys. These append-only results do not resolve that earlier regression.

## Grouped-payload throughput

All rates are MiB/s. Writes include synchronization every 16 MiB. The eight-reader rate is **per reader**, calculated from the time for each thread to open, read the entire file once, and validate it. Writing is not concurrent. Reads use a warm operating-system file cache.

| Payload/file | Path | Write | Sequential read | Each of 8 readers | Open/query preparation (ms) |
|---|---|---:|---:|---:|---:|
| 64 KiB / 1 GiB | raw | 1406.4 | 2495.0 | 976.5 | 0.013 |
| 64 KiB / 1 GiB | hdf5 | 1017.4 | 1688.3 | 298.8 | 0.354 |
| 64 KiB / 1 GiB | hdf5-fletcher | 738.2 | 1083.2 | 153.8 | 0.229 |
| 64 KiB / 1 GiB | sqlite | 333.1 | 1747.2 | 702.2 | 0.253 |
| 64 KiB / 1 GiB | varlit-native | 1021.1 | 1785.3 | 630.4 | 48.157 |
| 64 KiB / 1 GiB | varlit-stream | 1072.3 | 2034.0 | 672.9 | 0.072 |
| 64 KiB / 1 GiB | varlit-indexed | 845.3 | 1939.5 | 695.6 | 0.077 |
| 1024 KiB / 1 GiB | raw | 1436.9 | 2247.4 | 915.4 | 0.014 |
| 1024 KiB / 1 GiB | hdf5 | 1180.9 | 2089.7 | 644.6 | 0.282 |
| 1024 KiB / 1 GiB | hdf5-fletcher | 799.2 | 1138.2 | 190.6 | 0.284 |
| 1024 KiB / 1 GiB | sqlite | 361.0 | 1978.7 | 761.3 | 0.257 |
| 1024 KiB / 1 GiB | varlit-native | 1235.4 | 2100.7 | 788.2 | 2.704 |
| 1024 KiB / 1 GiB | varlit-stream | 1005.2 | 1998.5 | 714.1 | 0.072 |
| 1024 KiB / 1 GiB | varlit-indexed | 967.3 | 1982.1 | 767.1 | 0.071 |
| 4096 KiB / 4 GiB | raw | 1415.0 | 2471.3 | 915.3 | 0.015 |
| 4096 KiB / 4 GiB | hdf5 | 1146.3 | 1872.4 | 424.2 | 0.255 |
| 4096 KiB / 4 GiB | hdf5-fletcher | 651.7 | 1198.0 | 199.5 | 0.442 |
| 4096 KiB / 4 GiB | sqlite | 331.5 | 2164.5 | 746.1 | 0.261 |
| 4096 KiB / 4 GiB | varlit-native | 1278.7 | 2075.1 | 647.7 | 5.141 |
| 4096 KiB / 4 GiB | varlit-stream | 996.9 | 1987.2 | 648.2 | 0.073 |
| 4096 KiB / 4 GiB | varlit-indexed | 1033.7 | 2004.5 | 598.5 | 0.074 |

At 100,000,000 samples/s, 16/32/64-bit samples require **190.7 / 381.5 / 762.9 MiB/s** of payload respectively. Framing, application processing, and device bandwidth are additional. Full-byte pattern validation is application work in this experiment, so these rates are not pure library limits. Concurrent writer/reader results from the earlier Varlit-only report are separate; this study does not run the same SWMR comparison across external engines.

## Small records and random access

Times cover each complete operation. Random access uses the same seed to choose rows/keys, reads the complete payload, and validates it with the same Rust function. Stream lacks the same point API and has no random-access measurement. Native open timing includes extracting a reusable block-entry cache. Indexed/SQLite perform key lookup; raw/HDF5 calculate dense ordinals. These are not equivalent general-purpose key-index operations.

| Condition | Path | Write(ms) | Open/query preparation (ms) | Sequential read(ms) | Mean random access (µs) | Allocated files (MiB) |
|---|---|---:|---:|---:|---:|---:|
| small-64m-4k | raw | 47.358 | 0.010 | 26.523 | 2.088 | 64.004 |
| small-64m-4k | hdf5 | 224.961 | 0.234 | 76.243 | 5.535 | 64.742 |
| small-64m-4k | hdf5-fletcher | 158.287 | 0.217 | 101.204 | 7.377 | 64.805 |
| small-64m-4k | sqlite | 268.192 | 0.329 | 51.657 | 5.333 | 72.027 |
| small-64m-4k | varlit-native | 115.900 | 42.063 | 55.463 | 3.919 | 65.758 |
| small-64m-4k | varlit-stream | 83.734 | 0.082 | 60.013 | — | 65.785 |
| small-64m-4k | varlit-indexed | 91.927 | 0.072 | 59.913 | 5.032 | 69.820 |
| tiny-16m-128 | raw | 13.638 | 0.012 | 44.069 | 0.473 | 16.000 |
| tiny-16m-128 | hdf5 | 395.689 | 0.320 | 287.738 | 3.479 | 21.848 |
| tiny-16m-128 | hdf5-fletcher | 424.718 | 0.319 | 290.209 | 3.867 | 22.348 |
| tiny-16m-128 | sqlite | 131.908 | 0.239 | 30.545 | 2.974 | 17.707 |
| tiny-16m-128 | varlit-native | 336.795 | 199.635 | 136.827 | 1.761 | 30.004 |
| tiny-16m-128 | varlit-stream | 113.222 | 0.069 | 208.388 | — | 30.035 |
| tiny-16m-128 | varlit-indexed | 344.402 | 0.079 | 193.594 | 4.923 | 58.785 |

## CPU, memory, and writes

CPU values are process CPU seconds for the named stage. Eight-reader CPU sums all eight threads. RSS (resident set size) is the maximum across three repetitions and includes preparation and read stages. Operating-system write accounting includes WAL, sidecars, filesystem-accounted blocks, and logs.

| Condition | Path | Write CPU (s) | Sequential-read CPU (s) | 8-reader CPU (s) | Peak RSS (MiB) | OS writes (MiB) | Final files (MiB) |
|---|---|---:|---:|---:|---:|---:|---:|
| bulk-1g-64k | raw | 0.252 | 0.410 | 4.098 | 10.34 | 1024.01 | 1024.00 |
| bulk-1g-64k | hdf5 | 0.518 | 0.606 | 7.762 | 36.01 | 1027.05 | 1024.74 |
| bulk-1g-64k | hdf5-fletcher | 0.889 | 0.945 | 10.533 | 48.55 | 1026.35 | 1024.80 |
| bulk-1g-64k | sqlite | 2.032 | 0.586 | 5.624 | 73.39 | 2072.05 | 1032.03 |
| bulk-1g-64k | varlit-native | 0.491 | 0.572 | 6.379 | 25.32 | 1026.27 | 1025.76 |
| bulk-1g-64k | varlit-stream | 0.401 | 0.503 | 5.942 | 13.45 | 1029.90 | 1025.86 |
| bulk-1g-64k | varlit-indexed | 0.609 | 0.528 | 5.661 | 15.90 | 1103.39 | 1099.36 |
| bulk-1g-1m | raw | 0.237 | 0.456 | 4.324 | 15.77 | 1024.00 | 1024.00 |
| bulk-1g-1m | hdf5 | 0.386 | 0.490 | 5.429 | 37.35 | 1025.09 | 1024.05 |
| bulk-1g-1m | hdf5-fletcher | 0.820 | 0.900 | 8.217 | 54.24 | 1025.11 | 1024.06 |
| bulk-1g-1m | sqlite | 1.761 | 0.518 | 5.145 | 26.91 | 2058.65 | 1025.35 |
| bulk-1g-1m | varlit-native | 0.346 | 0.487 | 5.110 | 26.54 | 1024.62 | 1024.12 |
| bulk-1g-1m | varlit-stream | 0.461 | 0.511 | 5.575 | 32.75 | 1028.26 | 1024.22 |
| bulk-1g-1m | varlit-indexed | 0.471 | 0.516 | 5.127 | 32.77 | 1029.13 | 1025.09 |
| bulk-4g-4m | raw | 1.008 | 1.656 | 17.311 | 42.68 | 4096.00 | 4096.00 |
| bulk-4g-4m | hdf5 | 1.745 | 2.187 | 23.041 | 129.11 | 4100.19 | 4096.05 |
| bulk-4g-4m | hdf5-fletcher | 4.469 | 3.417 | 32.731 | 125.38 | 4100.11 | 4096.06 |
| bulk-4g-4m | sqlite | 8.162 | 1.892 | 21.658 | 44.67 | 8231.15 | 4100.51 |
| bulk-4g-4m | varlit-native | 1.373 | 1.973 | 24.853 | 80.54 | 4098.14 | 4096.13 |
| bulk-4g-4m | varlit-stream | 2.010 | 2.061 | 24.829 | 92.68 | 4112.48 | 4096.45 |
| bulk-4g-4m | varlit-indexed | 1.904 | 2.039 | 27.003 | 92.75 | 4115.34 | 4099.31 |
| small-64m-4k | raw | 0.019 | 0.027 | 0.265 | 10.44 | 64.00 | 64.00 |
| small-64m-4k | hdf5 | 0.189 | 0.076 | 2.656 | 24.84 | 64.80 | 64.74 |
| small-64m-4k | hdf5-fletcher | 0.124 | 0.101 | 3.094 | 31.62 | 64.87 | 64.80 |
| small-64m-4k | sqlite | 0.195 | 0.052 | 0.777 | 72.31 | 144.75 | 72.03 |
| small-64m-4k | varlit-native | 0.080 | 0.055 | 0.796 | 24.14 | 65.79 | 65.76 |
| small-64m-4k | varlit-stream | 0.045 | 0.060 | 0.575 | 11.92 | 66.07 | 65.79 |
| small-64m-4k | varlit-indexed | 0.057 | 0.060 | 0.529 | 15.18 | 70.11 | 69.82 |
| tiny-16m-128 | raw | 0.007 | 0.044 | 0.549 | 10.41 | 16.00 | 16.00 |
| tiny-16m-128 | hdf5 | 0.385 | 0.288 | 16.305 | 67.61 | 21.86 | 21.85 |
| tiny-16m-128 | hdf5-fletcher | 0.413 | 0.290 | 17.796 | 67.04 | 22.36 | 22.35 |
| tiny-16m-128 | sqlite | 0.109 | 0.031 | 0.372 | 73.77 | 35.68 | 17.71 |
| tiny-16m-128 | varlit-native | 0.316 | 0.137 | 3.159 | 144.56 | 102.02 | 30.00 |
| tiny-16m-128 | varlit-stream | 0.097 | 0.208 | 2.100 | 12.33 | 30.15 | 30.04 |
| tiny-16m-128 | varlit-indexed | 0.317 | 0.194 | 2.386 | 22.56 | 58.90 | 58.79 |

Separate probes sampled files and deleted open files every 5 ms after official timings. These non-atomic observations may miss brief peaks. Probe elapsed times are not pooled into benchmark timings.

| Condition | Path | Peak observed files + deleted files (MiB) | Deleted open files (MiB) |
|---|---|---:|---:|
| 1g-64k | raw | 1024.00 | 0.00 |
| 1g-64k | hdf5 | 1024.74 | 0.00 |
| 1g-64k | hdf5-fletcher | 1024.80 | 0.00 |
| 1g-64k | sqlite | 1048.34 | 0.00 |
| 1g-64k | varlit-native | 1025.77 | 0.00 |
| 1g-64k | varlit-stream | 1025.86 | 0.00 |
| 1g-64k | varlit-indexed | 1099.37 | 0.00 |
| 4g-4m | sqlite | 4116.70 | 0.00 |
| 4g-4m | varlit-indexed | 4099.31 | 0.00 |
| 4g-4m | hdf5-fletcher | 4096.06 | 0.00 |

## Conditions and interpretation

- Budgets remained a four-CPU quota with the same four logical CPUs in the affinity set, 16 GiB memory, and a 32 GiB filesystem. Synchronization occurs every 16 MiB, Varlit batches are 4 MiB, and engine cache settings are 8 MiB. The native resident directory has a separate default policy; equal cache options do not imply equal RSS.
- One Rust binary calls the official HDF5/SQLite C APIs directly. The comparison does not pit Python bindings or different-language loops against each other. Input generation and the same full-payload validation are included for every path.
- Varlit uses CRC32; HDF5 has unfiltered and Fletcher32 conditions. Raw and SQLite payloads do not have the same CRC32 contract. Checksum algorithms, recovery, and publication semantics differ. Raw is a baseline without structural/recovery features, not a proof of a physical performance limit. These engines are not established to have equal failure tolerance.
- HDF5 uses an extendable 2D uint8 dataset, one chunk per record, and an 8 MiB chunk cache. SQLite uses INTEGER PRIMARY KEY+BLOB, write-ahead logging (WAL), synchronous=FULL, an 8 MiB page cache, and a 5 s busy timeout. Varlit returns typed Vec payloads; SQLite's C result buffer is validated directly. Each API's reuse and copy costs are therefore included.
- HDF5 is a thread-safe build, with eight reader threads in one process. This does not represent multiprocess HDF5, dedicated mmap/zero-copy paths, compression, column projection, or all Arrow/Parquet/TDMS workloads.
- Each condition has three fixed repetitions. Primary order was rotated/reversed; the 15 corrected SQLite observations ran afterward, so interleaving with other engines was not preserved. Medians and all minima/maxima are in results.json. This is not the earlier 5% regression certification; small differences do not establish a definitive ranking. Builds, compression, and resource probes did not overlap official timings.
- 1 GiB / 64 KiB contains 16,384 records; 1 GiB / 1 MiB and 4 GiB / 4 MiB each contain 1,024. The smaller diagnostics contain 16,384 records for 64 MiB / 4 KiB and 131,072 for 16 MiB / 128 B.
- Initial native-policy and lazy RecordMap misuse in the harness were corrected during smoke checks; the interrupted attempt remains preserved. During primary measurements, SQLite's default zero busy timeout caused three reader-startup BUSY failures. A separate binary with a standard busy handler and correct STATIC-binding release reran **all 15 SQLite observations, including previously successful ones**. The original 105 attempts remain separately preserved; SQLite samples from the two versions are not pooled.
- Product code, APIs, and file formats are unchanged. Existing regressions and optimization work remain tracked in issue #20.

[Complete numbers and raw observations](results.json), [sources and all attempts](evidence.tar.gz), [per-file hashes](evidence-manifest.json), and [archive checksum](evidence.sha256).

## Reproduction

The evidence archive contains the standalone benchmark crate, C measurement parent, execution plans, and all stdout/stderr. `original-harness/main.rs` is the primary source; `src/main.rs` adds the corrected SQLite busy handling and binding release. Source and binary hashes are recorded in the corresponding plans.

Provide the system HDF5/SQLite libraries, adjust the Varlit dependency path in `Cargo.toml` and the native library search path in `build.rs`, and build the crate. Compile `measure.c` with a C compiler. Individual runs take arguments in this order: `mode /empty/work/directory total-payload-bytes record-payload-bytes reader-count random-query-payload-bytes`. Exact values are in `plan.json` and each observation's command. The product build does not include this Linux-only diagnostic crate.
