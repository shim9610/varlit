# Independent read checks: measurement appendix

This appendix is part of the [canonical combined workload report](../workload-storage-20261104/README.md) and its [integrated dashboard](../workload-storage-20261104/index.html). It adds before/after/mode measurements without replacing the corrected grouped-item comparisons or pooling their medians. No new cross-engine speed qualification is inferred.

## Contract and baselines

[ReadValidation](../../design/read-validation-policy-review.md) independently selects record/cell data CRC and additional metadata checks. `None` retains compatibility behavior; FULL, CRC_ONLY, STRUCTURE_ONLY and TRUSTED are explicit selections. Writer schema, stored CRCs, publication cadence, query work and resource budgets are held fixed. Required bounds, decoder limits, finite traversal and committed-publication guards remain in every mode. Generation/commit page checksums remain: disabling data CRC does not remove all CRC instructions.

The common retained Rust harness is compiled against pre-PR main `ba541cc3082cecdf7f385d06897a58a3889e1294`, before-change PR `82e10774faabc13eb549f4ecb56ecb9817465bc0` and the new runtime. Pre-PR main supports the six mixed profiles; its missing mapped-array API is not imitated. `before-a` and `before-b` use the **same binary** and separate measurement slots. `after-default` leaves the override unset. The old snapshots disable only the benchmark's new API-selection feature; production integrity/mmap dependency features remain the same. Exact inputs, source fingerprints and failed/interrupted attempts are retained in the evidence archive.

The final primary cohort has **69 configurations, 207 balanced timing observations, 69 separate sampled resource probes and 48 prefix-visibility gates**. Every reader consumes every published byte. Grouped blocks contain 512 or 8,192 logical 128-byte items; publications contain 16 MiB. Four producers use a bounded MPSC queue and one writer. These remain group-local queries, not arbitrary cold gets. Variable mixed payloads total approximately 768 MiB despite their nominal 1 GiB maximum-size configuration. Static array cases process 1 GiB; mmap cases perform two scans and cannot be compared as equal total work to one-scan cases.

All runs retain **4 CPU / 16 GiB / 32 GiB**, with affinity on four CPUs. Timing excludes resource-sampler overhead; process-tree wait4 CPU, full oracle work, copies, tail drain, allocated storage, sampled aggregate PSS, temporary files and deleted-open files remain charged. Cache state is warm. Timing phases overlap and must not be added. Read-active includes consume/refresh, queries and fixture verification. Publication-delay bounds are helper completion bounds, not individual item or exact commit latency.

## Primary results

Values below are medians from three timing observations. PSS is from one separate sampled probe per configuration and can miss brief peaks. Rows from another cohort are never used as a SQLite denominator. See [comparison.json](comparison.json) for ranges, concurrent progress, per-reader signatures and control diagnostics.

### grouped-n1048576-g512-t131072-w1-r1-varlit-indexed

| Runtime / policy | write s | read_active s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| pre-security | 0.1444 | 0.1179 | 0.1749 | 0.2461 | 130.004 | 77.96 |
| before-a | 0.1535 | 0.1670 | 0.2169 | 0.2946 | 130.000 | 77.99 |
| before-b | 0.1567 | 0.1712 | 0.2281 | 0.3120 | 130.000 | 78.32 |
| after-default | 0.1592 | 0.1336 | 0.1928 | 0.2625 | 130.000 | 78.00 |
| full | 0.1528 | 0.1330 | 0.1905 | 0.2616 | 130.000 | 78.15 |
| crc-only | 0.1501 | 0.1340 | 0.1868 | 0.2670 | 130.000 | 78.00 |
| structure-only | 0.1480 | 0.1267 | 0.1790 | 0.2525 | 130.000 | 77.97 |
| trusted | 0.1458 | 0.1199 | 0.1758 | 0.2491 | 130.000 | 78.15 |

### grouped-n8388608-g512-t131072-w1-r8-varlit-indexed

| Runtime / policy | write s | read_active s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| pre-security | 2.1253 | 2.5356 | 2.6211 | 9.9977 | 1102.035 | 108.66 |
| before-a | 2.2168 | 3.4190 | 3.5172 | 13.5454 | 1102.035 | 104.33 |
| before-b | 2.0152 | 3.2127 | 3.2970 | 12.5026 | 1102.035 | 104.42 |
| after-default | 2.2452 | 2.8398 | 2.9818 | 11.0556 | 1102.035 | 103.53 |
| full | 2.1335 | 2.5680 | 2.6663 | 10.1318 | 1102.035 | 103.34 |
| crc-only | 2.3369 | 2.7528 | 2.8532 | 10.8605 | 1102.035 | 104.20 |
| structure-only | 2.2038 | 2.5687 | 2.6548 | 10.0728 | 1102.035 | 103.95 |
| trusted | 2.1517 | 2.4176 | 2.5351 | 9.4887 | 1102.035 | 103.79 |

### grouped-n8388608-g512-t131072-w4-r8-varlit-indexed

| Runtime / policy | write s | read_active s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| pre-security | 2.1947 | 2.6775 | 2.7921 | 10.3647 | 1101.242 | 191.83 |
| before-a | 2.1473 | 3.3596 | 3.4508 | 13.2398 | 1101.105 | 173.80 |
| before-b | 2.1690 | 3.4849 | 3.5653 | 13.4249 | 1101.891 | 167.77 |
| after-default | 2.4324 | 2.6295 | 2.7481 | 10.5105 | 1102.047 | 187.75 |
| full | 2.3955 | 2.9988 | 3.0916 | 11.7919 | 1101.887 | 168.64 |
| crc-only | 2.6067 | 2.7813 | 2.9262 | 11.2216 | 1101.195 | 174.27 |
| structure-only | 2.5406 | 3.1338 | 3.2401 | 11.8015 | 1102.902 | 169.70 |
| trusted | 2.6701 | 2.6413 | 2.7685 | 10.7022 | 1101.156 | 168.83 |

### grouped-n8388608-g8192-t131072-w4-r8-varlit-indexed

| Runtime / policy | write s | read_active s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| pre-security | 1.7222 | 4.0990 | 4.2056 | 15.8587 | 1026.180 | 213.12 |
| before-a | 1.7722 | 4.7245 | 4.8209 | 18.8470 | 1026.184 | 192.15 |
| before-b | 1.7783 | 5.0904 | 5.2120 | 20.0511 | 1026.184 | 188.39 |
| after-default | 1.7139 | 3.8433 | 3.9516 | 15.1268 | 1026.234 | 191.69 |
| full | 1.6964 | 3.6806 | 3.7894 | 14.6319 | 1026.180 | 203.81 |
| crc-only | 1.7748 | 3.7741 | 3.8646 | 14.8452 | 1026.184 | 191.81 |
| structure-only | 1.6503 | 3.9228 | 4.0293 | 15.5616 | 1026.289 | 190.52 |
| trusted | 1.7887 | 4.1221 | 4.2094 | 15.9657 | 1026.168 | 189.78 |

### grouped-follow-n8388608-g512-t131072-w4-r8-varlit-stream

| Runtime / policy | write s | read_active s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| pre-security | 1.6070 | 0.9971 | 1.6507 | 5.1491 | 1026.516 | 171.74 |
| before-a | 1.5714 | 0.9950 | 1.6110 | 4.9889 | 1026.516 | 155.46 |
| before-b | 1.5195 | 0.9356 | 1.5614 | 4.8647 | 1026.516 | 171.41 |
| after-default | 1.5555 | 1.0242 | 1.5848 | 4.9536 | 1026.520 | 155.40 |
| full | 1.7233 | 1.0502 | 1.7881 | 5.7364 | 1026.516 | 155.40 |
| crc-only | 1.4574 | 0.8574 | 1.5003 | 4.6099 | 1026.516 | 155.41 |
| structure-only | 1.5005 | 0.8975 | 1.5409 | 4.6890 | 1026.516 | 155.41 |
| trusted | 1.5068 | 0.8656 | 1.5500 | 4.6580 | 1026.516 | 155.35 |

### mixed-variable-p65536-w4-r8-varlit-indexed

| Runtime / policy | write s | read_active s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|
| pre-security | 1.4804 | 1.9629 | 2.0780 | 7.8663 | 794.395 | 134.23 |
| before-a | 1.4328 | 1.9877 | 2.0574 | 7.5802 | 795.680 | 140.65 |
| before-b | 1.5989 | 2.0574 | 2.1292 | 8.0273 | 794.621 | 133.49 |
| after-default | 1.4767 | 1.9194 | 2.0176 | 7.5502 | 794.113 | 136.72 |
| full | 1.5248 | 1.8256 | 1.9143 | 7.2410 | 794.680 | 129.28 |
| crc-only | 1.5548 | 1.8430 | 2.0442 | 7.4614 | 794.703 | 129.22 |
| structure-only | 1.5971 | 1.8636 | 1.9737 | 7.5285 | 794.445 | 134.07 |
| trusted | 1.5518 | 1.8305 | 1.9157 | 7.1813 | 794.414 | 136.69 |

### array-native-owned-n1073741824-g1048576

| Runtime / policy | write s | scan s | warm_scan s | point s | range s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| before-a | 0.8131 | 0.1612 | — | 0.5318 | 0.0267 | 1.7131 | 1.2332 | 1024.121 | 24.89 |
| before-b | 0.8200 | 0.1670 | — | 0.5574 | 0.0270 | 1.7112 | 1.2168 | 1024.121 | 24.96 |
| after-default | 0.8440 | 0.1617 | — | 0.5381 | 0.0319 | 1.7154 | 1.2103 | 1024.121 | 24.89 |
| full | 0.8866 | 0.1736 | — | 0.5424 | 0.0256 | 1.7523 | 1.2675 | 1024.121 | 24.90 |
| crc-only | 0.8981 | 0.1664 | — | 0.5200 | 0.0269 | 1.7621 | 1.2673 | 1024.121 | 24.96 |
| structure-only | 0.8511 | 0.1357 | — | 0.4339 | 0.0209 | 1.5604 | 1.0767 | 1024.121 | 24.89 |
| trusted | 0.8266 | 0.1316 | — | 0.4155 | 0.0216 | 1.5371 | 1.0634 | 1024.121 | 24.96 |

### array-native-mmap-n1073741824-g1048576

| Runtime / policy | write s | scan s | warm_scan s | point s | range s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| before-a | 0.8458 | 0.0936 | 0.0629 | 0.0033 | 0.0044 | 1.2168 | 0.7103 | 1024.121 | 1036.14 |
| before-b | 0.8153 | 0.1047 | 0.0554 | 0.0031 | 0.0045 | 1.1819 | 0.6969 | 1024.121 | 1036.02 |
| after-default | 0.8189 | 0.0913 | 0.0530 | 0.0028 | 0.0041 | 1.1685 | 0.6689 | 1024.121 | 1036.09 |
| full | 0.8050 | 0.1026 | 0.0573 | 0.0027 | 0.0045 | 1.1556 | 0.6842 | 1024.121 | 1036.15 |
| crc-only | 0.8295 | 0.1005 | 0.0574 | 0.0027 | 0.0044 | 1.1645 | 0.6787 | 1024.121 | 1036.09 |
| structure-only | 0.8535 | 0.0779 | 0.0548 | 0.0029 | 0.0044 | 1.1836 | 0.6916 | 1024.121 | 1036.15 |
| trusted | 0.8148 | 0.0911 | 0.0600 | 0.0029 | 0.0054 | 1.1563 | 0.6754 | 1024.121 | 1036.09 |

### array-matrix-owned-n1073741824-g1048576

| Runtime / policy | write s | scan s | warm_scan s | point s | range s | completion s | cpu s | Final MiB | PSS MiB |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| before-a | 2.2285 | 0.4042 | — | 1.5338 | 0.0742 | 4.3976 | 3.8687 | 1034.801 | 25.31 |
| before-b | 2.2605 | 0.4305 | — | 1.6516 | 0.0844 | 4.4684 | 3.9231 | 1034.801 | 25.39 |
| after-default | 2.2701 | 0.4123 | — | 1.6087 | 0.0784 | 4.5187 | 4.0038 | 1034.801 | 25.34 |
| full | 2.2409 | 0.3986 | — | 1.5524 | 0.0797 | 4.3865 | 3.8719 | 1034.801 | 25.32 |
| crc-only | 2.2437 | 0.3950 | — | 1.5196 | 0.0732 | 4.3767 | 3.8387 | 1034.801 | 25.22 |
| structure-only | 2.2326 | 0.4051 | — | 1.6155 | 0.0694 | 4.5949 | 4.0617 | 1034.801 | 25.34 |
| trusted | 2.2577 | 0.3823 | — | 1.5788 | 0.0715 | 4.4139 | 3.8861 | 1034.801 | 25.39 |

## Qualification and retained alerts

The initial A/A-median-only analysis reported 50 within / 33 unqualified / 5 exceeding observations across 88 metric comparisons. Those five alerts remain visible in `aa_median_status` and `observed_exceedance`: native-owned range (+18–19%), grouped-512 four-producer writer (+12–13%), its sampled PSS (+8–12%), stream p99 upper delay (+17–20%), and variable-mixed drain (approximately +7%). They are not converted to passing results.

Comparing A/A medians alone can miss substantial variation inside either baseline. The final analysis also reports each baseline's full repeat spread and refuses a 5% qualification when that spread exceeds 5%. A single resource sample cannot qualify a memory/storage regression. With this stricter calibration, **9 comparisons are within and 79 are unqualified**. This is a measurement limitation, not a claim that all regressions disappeared. No library-wide 5% or 2× SQLite claim is established. All original ratios, ranges and alerts remain in the data.

For the grouped-512 four-producer/eight-reader case, default writer wall time rises from 2.147/2.169 s to 2.432 s, while writer thread CPU is 0.768/0.764/0.766 s. Aggregate reader CPU drops from 12.23/12.43 s to 9.48 s; reader payload completed entirely inside the writer window increases from 3.64/3.72 GiB to 5.81 GiB across eight readers. Whole completion improves from 3.451/3.565 s to 2.748 s. This identifies a concurrent-overlap/off-CPU component; it does **not** isolate a specific scheduler or filesystem lock. Writer-only diagnostics below do not reproduce a writer slowdown. Publication pacing was not changed to make a metric pass.

Peak temporary allocated storage across the primary probes is 0.0274 MiB; sampled deleted-open storage is zero. Cgroup OOM and OOM-kill counters do not increase. This is observed resource behavior, not proof that process-wide OOM is impossible.

## Supplemental diagnostic cohorts

These are **144 additional balanced timing observations and 26 resource probes** with the same caps, kept separate from the primary cohort. Six timing repeats are retained per condition in [diagnostics.json](diagnostics.json).

### Captured native payload API

A 1 GiB file is read three times into a pre-reserved caller-owned Vec using `read_payload_into`. The timer covers the actual library call; full-byte fixture verification is timed separately and remains in whole-process CPU/completion. Encoded bytes, user bytes and API CPU are distinct fields. These are warm, uncompressed native reads, not a claim about every query/decompression API.

| Block bytes | Runtime / mode | API wall s | API CPU s | Oracle s | Full / API wall | Sampled PSS MiB |
|---:|---|---:|---:|---:|---:|---:|
| 65536 | before-a | 0.563105 | 0.575289 | 1.265041 | 0.986 | 4.47 |
| 65536 | before-b | 0.551298 | 0.562601 | 1.234495 | 1.008 | 4.41 |
| 65536 | after-default | 0.564209 | 0.575425 | 1.404115 | 0.985 | 4.35 |
| 65536 | full | 0.555465 | 0.567089 | 1.367012 | 1.000 | 4.35 |
| 65536 | crc-only | 0.531582 | 0.542928 | 1.369828 | 1.045 | 4.41 |
| 65536 | structure-only | 0.498684 | 0.511561 | 1.291659 | 1.114 | 4.47 |
| 65536 | trusted | 0.458212 | 0.472669 | 1.268362 | 1.212 | 4.41 |
| 1048576 | before-a | 0.329847 | 0.332265 | 1.289662 | 0.979 | 6.51 |
| 1048576 | before-b | 0.323397 | 0.326037 | 1.256812 | 0.999 | 6.63 |
| 1048576 | after-default | 0.334342 | 0.336431 | 1.431834 | 0.966 | 6.57 |
| 1048576 | full | 0.322948 | 0.324781 | 1.403977 | 1.000 | 6.63 |
| 1048576 | crc-only | 0.331843 | 0.333808 | 1.434942 | 0.973 | 6.50 |
| 1048576 | structure-only | 0.255059 | 0.256661 | 1.308397 | 1.266 | 6.63 |
| 1048576 | trusted | 0.246311 | 0.248071 | 1.273475 | 1.311 | 6.57 |

TRUSTED takes about 18% / 24% less API wall time than FULL at 64 KiB / 1 MiB in these observed medians. Default API CPU differs by at most about 3.2% from the two before controls. Repetition spreads still prevent a 5% qualification. CPU timer calls and the fixture oracle have their own overhead; they are not subtracted from whole-process results.

### Writer-only and longer range control

The writer-only cohort retains the same grouped 1 GiB/four-producer input and bounded queue, with zero concurrent readers. After the timed writer phase it performs a separately timed full readback and group-local queries; whole-process CPU includes that verification. The range cohort performs 2,048 native-owned windows rather than the primary 128, holding that larger work equal across variants. Its medians are not pooled with the primary ranges.

| Runtime / mode | Writer ingest s | Writer thread CPU s | Separate readback s | 2,048 windows s |
|---|---:|---:|---:|---:|
| before-a | 1.309830 | 0.703273 | 0.974476 | 0.433889 |
| before-b | 1.303024 | 0.693254 | 0.985199 | 0.408719 |
| after-default | 1.284240 | 0.676787 | 1.267144 | 0.432380 |
| full | 1.279174 | 0.666000 | 1.209009 | 0.417234 |
| trusted | 1.359992 | 0.728652 | 1.176191 | 0.347716 |

Default writer-only ingest is about 1.4–2.0% shorter than before. The longer range control is −0.3% / +5.8% against the two before slots; those controls themselves differ by about 6.2%, so the primary +18–19% range alert is not reproduced as a stable effect. The readback diagnostic has an observed default increase of 29–30%, including full fixture checking and group-local item queries; it remains an explicit alert, with the separate attribution below.

### Readback attribution (instrumented, separate)

Twelve additional runs measure five consume stages, three repeats per runtime/mode. Instrumentation changes code layout and adds timers, so these are attribution evidence, not replacements for uninstrumented timings or 5% qualification.

| Runtime / mode | Cursor next s | Frame oracle s | Indexed get s | Equality s | Point copy/oracle s |
|---|---:|---:|---:|---:|---:|
| before-legacy | 0.168770 | 0.195067 | 0.159772 | 0.020323 | 0.375062 |
| after-legacy | 0.166952 | 0.195784 | 0.170848 | 0.019864 | 0.657038 |
| after-full | 0.172721 | 0.195208 | 0.174655 | 0.020451 | 0.667924 |
| after-trusted | 0.139409 | 0.189866 | 0.118386 | 0.018981 | 0.641306 |

Most of the observed readback increase is in the benchmark's point copy/full-byte oracle (about +0.282 s); cursor-next is comparable. Indexed-get in the instrumented default has a smaller observed +0.011 s (+6.9%) and overlapping individual ranges, retained rather than declared equivalent. TRUSTED reduces cursor-next and get medians. The uninstrumented before/after `Oracle::point_payload` functions are both 579 bytes and have the same successful-path instructions, with different addresses/alignment. No additional library validation instructions execute inside that benchmark-only oracle. The evidence does not establish a specific frontend/cache cause, and the whole-process readback alert is not discarded or “corrected” by subtracting the oracle.

A separate `strace` I/O audit (one per variant; tracer timings excluded) observes approximately 239k positional reads for before/default/FULL and 140k for TRUSTED across the writer plus readback process. Publication/interleaving can change writer-side index-page counts. Exact captured-entry counts are asserted by the core tests; these process-wide syscall totals are supplemental observations, not exact per-query contracts.

### Repeated sampled memory

Three separate resource probes repeat the grouped-512 four-producer/eight-reader workload. PSS samples are aggregate process-tree maxima, not synchronized with one another or with temp maxima.

| Runtime / mode | PSS observations MiB | Median MiB |
|---|---|---:|
| before-a | 167.52, 168.80, 166.90 | 167.52 |
| after-default | 173.23, 176.85, 168.88 | 173.23 |
| full | 168.58, 173.01, 173.35 | 173.01 |
| trusted | 168.27, 169.21, 189.34 | 169.21 |

Default's repeated median is approximately 3.4% above the before median, rather than the single primary probe's +8–12%. These samples do not establish a 5% upper bound; TRUSTED has one higher observed peak. No cache or buffer cap was increased. The diagnostics' cgroup memory event counters are unchanged.

## Failures, corrections and verification

- Primary v1 stopped on the **unchanged before binary**: the conformance gate incorrectly assumed producer transaction ID zero was the first actual publication. A different MPSC producer legitimately published first. All adapters now gate the previous publication count; the prefix contract is unchanged. Raw failure, framing fixtures and binary hashes remain retained.
- Primary v2 was deliberately interrupted for the compatibility fix preserving no-CRC + AtOpen admission. Its results are excluded from v3, not combined into selected medians. Interrupted files and manifests remain retained.
- Diagnostic v1's writer-only request was rejected by the primary mixed CLI's requirement for at least one reader. A separate writer-only diagnostic validates its own shape without weakening the primary CLI. All diagnostic v1 rows are retained and excluded from v2 medians.
- An initial local debug-symbol-heavy build reached the fixed disk budget. Rebuildable compiler caches were removed and debug symbols disabled for local tests; no runtime/benchmark budget increased. The initial formal-tool invocation lacked goto-cc on PATH; the corrected toolchain rerun and initial error are retained.
- Local default and all-feature workspace suites pass (855 and 1,414 tests/doctests respectively; 20 and 30 intentionally ignored). Default/all-feature Clippy pass with warnings denied. The eight new integration tests cover policy combinations, allocator refusal/retry, native/indexed/mmap/compressed/matrix paths, pending versus committed publication with eight readers, and retained cursors. Core contracts count exact read calls and test the unset compatibility policy as well as all four explicit modes.
- Hostile disk-index shape/compound I/O/allocation tests execute with metadata checks both on and off. The resource Kani runner passes 19 baseline harnesses and detects 17/17 deliberate mutants. This is bounded resource/traversal verification, not a whole-owner initialization or process-wide OOM proof.

## Reproduction and evidence

Use the commands in the [benchmark manual](../../../tools/storage-bench/README.md). `compare_read_validation.py` runs the primary common harness; `diagnose_read_validation.py` runs the separate diagnostic cohorts; their two analyzers preserve all observations. The canonical dashboard uses the primary 69 measured profiles, while the diagnostic tables remain in this linked appendix.

[evidence.tar.gz](evidence.tar.gz) retains manifests, JSONL/raw observations, usage/probe counters, source snapshots, hashes, failed/interrupted attempts, compiler/test/proof logs and analysis inputs. Binaries and build caches are excluded; their SHA-256 identities are retained. The archive contains the exact primary controller before the later payload-diagnostic branch was added. [SHA256SUMS](SHA256SUMS) covers the published appendix. No Korean report is published in the repository.
