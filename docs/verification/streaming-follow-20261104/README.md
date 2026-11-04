# Streaming follow and bounded native mark hints

Runtime source: `5d8a852cc2690876e8ad52a680cce71d1707422f`. Pending reader progress and exact nonresident offsets are retained so a cooperating current writer does not make readers rescan old pending prefixes on every poll. Existing public signatures and primary/index formats remain compatible.

**Performance qualification: not complete; failed or unqualified final comparisons remain below.** The earlier failed and partial candidates are preserved in the evidence archive.

## Behavior and safety

Unchanged pending tails frame zero old records. Growth resumes at the last complete frame. A commit discovered using cached state triggers one authoritative replay of that pending batch before publication. Writer/lease changes, truncate and incompatible file state invalidate progress; legacy/unknown owners use conservative growth handling. Cached progress cannot authorize publication.

Full scans and successful appends retain exact 8-byte nonresident record offsets. The optional cache borrows at most 16 KiB from the existing directory allowance, then spills. Optional cache allocation/spill refusal disables acceleration without refusing logging. Replay reuses its staging storage, and reset stores its budget instead of an unused 664-byte FormatSpec. No default large-file cap or forced database spill directory is introduced.

Marking uses an untrusted sequence only to select a private row, and requires its actual offset to match. Both resident and spilled rows retain this check. The first sequence is loaded only when a mark needs the hint; ordinary appends/follow do not update sequence-hint state. A valid sequence-gap miss disables the hint for that generation and uses existing boundary lookup. The record-install and verified-offset-update functions match 67a8939 source; this is source-loop equality, not a universal timing guarantee.

## Validation

The final source passes 1,403 all-feature and 853 default tests/doctests (30/20 ignored), Clippy on all targets and formatting. Core 303 and focused streaming/liveness/lock/architecture 59 tests pass. A full-width Kani proof checks sequence subtraction/candidate conversion. A separate full-width index proof checks the exact 13-byte backing window within at most 39 bytes. Neither proves filesystem behavior or whole-library OOM impossibility. The pure window bounds/wrap contract passes Miri.

Actual-offset comparisons are independently removed in the resident and spilled paths; each spoofing test fails, restored source passes. Earlier cursor-retention, publication-replay, scanned-boundary and appended-boundary defects are also detected. The 55 existing Miri exclusions remain; native/ASan alternatives do not provide equivalent Miri guarantees. 

The preceding published 2e68aa9 passes all 71 [CI #109](https://github.com/shim9610/varlit/actions/runs/41767894100) jobs and all three [Sanitizers #84](https://github.com/shim9610/varlit/actions/runs/41767894092) jobs, including CodeQL, Kani default/all, ASan/leak, fuzz and selected full Miri. Exact-head checks for the subsequent source/report push must be recorded separately. Kani uses two workers within the same 4 CPU allowance and 30-minute timeout, and the strict checker still requires every expected successful harness exactly once.

## Equal-work mark measurements

Four CPUs, 16 GiB RAM and 32 GiB disk are unchanged. These mark experiments use one allowed logical CPU, matching the original-reference experiments. OriginalA/B are one identical 317fc981 executable; before is 67a8939. All labels use the same unchanged canonical harness, records, offsets/order, calls and post-timer verification. CPU includes preparation; operation time excludes it. Peak resident memory (RSS) is the whole child process maximum in KiB.

| Case | Groups | Original A/B (ms) | Before 67 (ms) | Final (ms) | All observed gates |
|---|---:|---:|---:|---:|---|
| variable | 12 | 40.49/41.97 | 37.69 | 36.00 | pass |
| short-mixed | 24 | 32.03/32.72 | 33.62 | 31.84 | pass |
| epochs | 24 | 29.77/27.89 | 26.84 | 27.66 | failed/unqualified |
| rewritten | 12 | 40.26/41.07 | 36.75 | 37.43 | pass |

The mixed file contains 4,306,436,326 bytes and 8,192 random marks. The other cases use 200,000 records and 20,000 marks, including periodic flushes and a rewritten generation. CPU/RSS/write/private-space results and identical-binary calibration are in results.json.

## Independent writer and readers

The writer never waits for reader acknowledgments during ingestion. This series constrains execution to four logical CPUs as well as the existing four-CPU quota; it is separate from the retained unrestricted-affinity failures. All payload bytes, calls, decoded work and hashes match. Internal discovery/replay counts can vary with scheduling.

| Case | Groups | Final completion (s) | Final writer (s) | Final CPU (s) | Final RSS (KiB) | All observed gates |
|---|---:|---:|---:|---:|---:|---|
| independent-1g-r8 | 12 | 1.488 | 0.762 | 5.678 | 8496 | failed/unqualified |
| independent-4g-r8 | 12 | 5.870 | 3.208 | 22.756 | 13548 | failed/unqualified |
| complex-varying | 12 | 1.473 | — | 4.180 | 14244 | pass |
| independent-1g-r1 | 12 | 0.661 | 0.373 | 1.116 | 3268 | pass |

Independent cases use 1/4 GiB, 64 KiB payloads, 16 MiB epochs and 16 follows per reader per epoch. The complex varying case uses 256 MiB, eight readers and varying record sizes near 4 KiB. Every initial measurement is retained; no observed regression threshold or resource allowance is raised.

Final failed/unqualified comparisons:

- marks/epochs: identical-binary calibration, seconds -6.32%.
- writer_readers/independent-1g-r8: identical-binary calibration, writer_seconds +16.38%.
- writer_readers/independent-1g-r8: observed limit exceeded, writer_seconds +16.57% versus before-a.
- writer_readers/independent-4g-r8: identical-binary calibration, writer_seconds +12.00%.
- writer_readers/independent-4g-r8: observed limit exceeded, writer_seconds +10.12% versus before-a.
- writer_partition/1g: identical-binary calibration, writer_seconds +9.21%.
- writer_partition/4g: identical-binary calibration, writer_seconds -9.58%.

## Writer CPU partition diagnostic (native-child resource accounting)

This additional scenario reserves one of the same four logical CPUs for the writer and shares the other three among eight readers. The binary and caller work are unchanged. Reader-thread affinity setup is observed externally; setup cost remains included and its duration is recorded. This is an application scheduling configuration, not a library affinity policy, and does not replace the shared-CPU failures. Six balanced groups cover one block of all six execution orders. Actual after/before metrics pass, but both identical-binary writer calibrations exceed 5%, so neither case is qualified.

| Payload | Final writer (s) | Final completion (s) | Final RSS (KiB) | All observed gates |
|---|---:|---:|---:|---|
| 1g | 0.409 | 1.910 | 9328 | failed/unqualified |
| 4g | 1.837 | 7.261 | 15048 | failed/unqualified |

## Separate resource probes

A small C wrapper forks and measures the native executable so pre-exec Python RSS is excluded. Two groups per case/version measure wait4 CPU, write bytes and maximum process RSS, plus 5 ms samples of deleted regular-file logical/allocated bytes, all open regular-file allocation and cgroup memory. The cgroup includes file cache and workspace/observer overhead. Sampled storage peaks are lower bounds. Observer-affected timings are not qualification timings. FD/status sample failures are preserved rather than interpreted as zero storage. Earlier direct-Python wait4 RSS results included the pre-exec parent high-water mark and are invalid for native memory comparisons; their raw data and correction are retained.

| Case | Version | Max RSS (KiB) | Sampled deleted logical/allocated (KiB) | Sampled open allocation (MiB) |
|---|---|---:|---:|---:|
| small | original | 39988 | 0.0/0.0 | 27.01 |
| small | budget | 33884 | 0.0/0.0 | 27.01 |
| small | final | 34720 | 0.0/0.0 | 27.01 |
| complex | original | 13308 | 0.0/0.0 | 261.51 |
| complex | budget | 14016 | 0.0/0.0 | 261.51 |
| complex | final | 14028 | 0.0/0.0 | 261.51 |
| nonresident | original | 3700 | 0.0/0.0 | 1025.39 |
| nonresident | budget | 3492 | 128.0/128.0 | 1025.51 |
| nonresident | final | 3564 | 128.0/128.0 | 1025.51 |
| independent-4g | original | 13104 | 0.0/0.0 | 4101.55 |
| independent-4g | budget | 14520 | 0.0/0.0 | 4101.55 |
| independent-4g | final | 15100 | 0.0/0.0 | 4101.55 |

Original is 5e480a5 and budget is 98692fa. Exact nonresident retention previously adds 128 KiB versus zero in the 1 GiB case (~0.012% of source+private bytes). The raw zero-baseline 5% private-space gate remains failed; it is not relabeled as a pass. Actual final sampled peaks are shown above.

## 100 million-samples/s diagnostic

Sample width remains unconfirmed. A separate paced diagnostic assumes 16/32/64-bit samples, 64 KiB batches and 16 MiB epochs, with eight readers and three fixed groups. It tests offered-load feasibility, not maximum-throughput equivalence or deadlines or loss at an external data source. All supplied bytes are logged and decoded; producer epoch lag and maximum push/commit call times remain visible.

| Bits/sample | Offered MiB/s | Final writer MiB/s range | Maximum reported lag (s) |
|---|---:|---:|---:|
| 16 | 190.735 | 190.73–190.73 | 0.000000 |
| 32 | 381.470 | 381.45–381.46 | 0.000000 |
| 64 | 762.939 | 760.05–762.90 | 0.014042 |

## Retained unsuccessful work

The earlier 5e480a5 original-reference mixed-mark pool still failed time by 5.62%/6.22% over 240 groups. Its later stages were not run. The initial replay implementation failed complex RSS and was replaced by storage reuse. A staging-growth attempt gave no RSS benefit and increased CPU about 1.5%, so it was rejected. The first sequence hint accidentally read 64 KiB per random point instead of 13 bytes and was rejected. Global point-read inlining passed mark tests but failed complex reader CPU; the final implementation borrows resident rows only on the mark path. Per-record sequence tracking was then removed entirely from ordinary logging.

Initial, fixed replication, rejected-candidate samples, plans, source patches and provenance remain in the archive. An earlier pair of control stdout files was overwritten by a helper naming collision; evidence-notes.json identifies the loss, and the final variant-specific controls are preserved. Performance samples are not affected.

See [results.json](results.json), [raw evidence](evidence.tar.gz), [per-file hashes](evidence-manifest.json), [archive checksum](evidence.sha256) and the [historical recovery report](../performance-recovery-20261103/README.md).
