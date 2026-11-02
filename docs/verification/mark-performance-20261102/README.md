# Record mutation performance follow-up

This follow-up addresses the reverse-writer operation-time failure reported at
`dad27af77d6356af8a2b6a78896290d0a27f8a8d`: **+14.83%** against
`598ec8b31dcf09d718955d901fa7bed4d2360eb2`. The historical measurement remains
in the compound-failure report. Earlier, differently scoped PR regressions are
not erased by this comparison. Regression checks pass; **timing qualification
remains inconclusive because the same-binary calibration exceeds 5%**.

## Implementation

`Option::ok_or(Error::...)` eagerly constructed unused stack error values on
successful reads. Rust 1.95.0 release code retained calls to the error enum's
destructor dispatch. Explicit `let Some(...) else { return Err(...) }` branches
now construct those errors only on failure. These were stack values, not heap
allocations.

The two separately tested and benchmarked units are:

1. Commit `b6833f2c7225460fb5837f8ef1488392c97c9c01`: checked offset/length
   arithmetic in record lookup, marking and snapshot reads. The compiled `mark_record_dead` loses three error-drop call sites;
   `SnapshotFile::read_exact_at` loses two.
2. Native header field extraction and footer flag extraction. Each of the
   three emitted header decoder specializations loses seven error-drop call
   sites. The footer loses its one unused success-path error; all six required
   error-conversion/cleanup call sites remain.

The canonical field-table decoder, arithmetic/range checks, failure variants,
short-read behavior and footer validation remain. No API, format, allocation
policy, buffer allowance or dependency changes. The separately planned ownership
library is outside this change.

## Regression checks

Each unit passed release core tests (**264 default / 278 all features**, three
ignored in each), **35 public regressions**, workspace/all-target/all-feature
Clippy with warnings denied, and formatting. AddressSanitizer with leak detection
passed **278 core / 27 public liveness tests** after each unit. Miri passed the
17 snapshot/extent tests for unit 1 and the five native-layout tests for unit 2,
using its ordinary alias/validity checks. These are targeted executed checks;
Kani was not rerun locally for this optimization.

The first `ok_or_else` prototype failed four Clippy unnecessary-lazy-evaluation
lints. Explicit branches replaced it; the failed log and prototype assembly are
retained. No lint suppression was added.

## Measurement protocol

All release examples use Rust 1.95.0, identical all-feature settings and the
unchanged workload harness. Each native case starts with five alternating pairs;
a metric above 5% triggers 15 more, pooling all 20. Single-writer/multiple-reader
(SWMR) runs use three alternating pairs at 1 and 4 GiB, eight following readers
plus a pinned reader, and 16,384 checks per reader per epoch. Work, snapshots and
traces must match. Whole-process CPU, maximum resident set size (RSS), writes and
separate sampled temporary-storage readings accompany operation/wall time.

The reverse fixture is exactly `writer large reverse 65536 4096`: a
4,300,734,694-byte source, 4,096 distinct marks and 4,096 frame faults. Its
operation timer excludes file creation, and lasts only around 15 ms. CPU readings
cover the whole process and therefore cannot substitute for that timer.

An initial 36-observation control accidentally used `/tmp` (tmpfs); it is
unqualified for the disk-backed question. A replacement 20-group, four-way
control used explicit disk scratch. Identical reference binaries differed
**12.71%** in their operation medians, so this control did not qualify the 5%
regression claim. Both controls are retained.

Before the final control started, its length was fixed at **200 randomized
four-way groups**: each group randomly orders reference A, reference B, the
intermediate binary and the final binary. Reference A and B are the same
`598ec8b` executable. Each sample keeps the original command and disk filesystem. There is no early stopping or selected
passing subset. Intermediate-to-final reverse-writer samples from the ordinary
suite are pooled with this control's corresponding samples. The two reference
labels separately calibrate timing stability and compare the final code to the
original baseline.

CPU and memory limits remain **four CPUs / 16 GiB**. Builds, tests and assembly
generation finish before timing begins. Resource probes are separate from timing;
unit 1's probes use the platform temporary directory (tmpfs), while unit 2's
probes explicitly use disk scratch. They are compared within their own unit. Two preliminary large-file resource
probes had an incorrect sampler key that reported zero cgroup values. Those
probes are retained as invalid diagnostics; replacement probes check the key set
and require nonzero current/file/anonymous memory readings before accepting data.

## Final measured results

**Overall timing qualification remains inconclusive.** The observed pooled
implementation comparisons are within the unchanged 5% gates, but identical
reference binaries differ by **6.83%**. The final comparison against reference A
has a 95% interval ending at **+5.38%**, so these measurements do not establish
a reliable 5% upper bound. The code removes verified unnecessary work; the
historical +14.83% result remains recorded. There are
**984 timing observations across the two units and the final control**, plus
**6 separate resource probes**. The additional 80-observation disk diagnostic
and 36-observation tmpfs diagnostic remain separately identified.

| Workload | Unit 1 pairs | Time | CPU | Unit 2 pairs | Time | CPU |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Fixed writer, 2M records | 5 | -6.10% | -0.53% | 5 | -5.41% | -2.03% |
| Large reverse writer | 5 | +3.48% | -3.71% | 220 | -1.17% | -0.06% |
| Complex maintenance, 65,536 × 512 B | 5 | +2.64% | -0.14% | 5 | -5.22% | -6.14% |
| Compression, 64 MiB | 5 | -2.10% | -1.55% | 5 | -3.04% | -3.50% |
| Decoding, 64 MiB | 20 | +0.43% | +0.74% | 5 | +0.70% | +3.75% |
| SWMR 1 GiB, eight followers + pin | 3 | -2.61% | -2.41% | 3 | -2.26% | -1.33% |
| SWMR 4 GiB, eight followers + pin | 3 | +2.08% | +1.47% | 3 | +1.08% | +1.51% |

Unit 1 compares `dad27af` to `b6833f2`; unit 2 compares `b6833f2` to the
final decoder change. These are separate predecessor comparisons, not additive
or cumulative speedup claims. Unit 1 decoding initially failed at +9.54% time /
+6.89% CPU; its prescribed 15 confirmation pairs bring the full 20-pair result
to the values above. Unit 2 reverse-writer time is **+12.63% across its ordinary
20 pairs**. All those samples remain in the final **220-pair** result above.

The final direct comparison to the original `598ec8b` reference is:

| Comparison (200 samples per label) | Operation medians (ms) | Time | CPU | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| reference-a -> reference-b | 13.066 → 13.958 | +6.83% | -0.08% | +0.11% |
| reference-a -> fixed | 13.066 → 13.246 | +1.38% | -0.30% | +0.22% |
| reference-b -> fixed | 13.958 → 13.246 | -5.10% | -0.22% | +0.11% |
| intermediate -> fixed | 13.563 → 13.246 | -2.33% | -0.15% | -0.11% |

Supplemental paired-group bootstrap 95% intervals for operation-time changes
(10,000 resamples; the configured gate still uses the recorded medians):

* reference-a -> reference-b: +0.80% to +9.59%.
* reference-a -> fixed: -2.00% to +5.38%.
* reference-b -> fixed: -8.02% to +0.91%.
* intermediate -> fixed: -5.57% to +2.46%.

Both complex resource comparisons retain **4,194,304 bytes** of sampled private
temporary allocation and **91,955,200 bytes** of open regular-file allocation.
The separate disk-backed large-file resource comparison records:

| Sampled peak | Original reference | Final |
| --- | ---: | ---: |
| rss_bytes | 3,690,496 | 3,710,976 |
| temporary_deleted_allocated_bytes | 0 | 0 |
| open_regular_allocated_bytes | 4,300,746,752 | 4,300,746,752 |
| cgroup_current_bytes | 11,959,336,960 | 11,959,140,352 |
| cgroup_file_bytes | 7,637,975,040 | 7,637,975,040 |
| cgroup_anon_bytes | 48,787,456 | 48,787,456 |
| cgroup_shmem_bytes | 1,425,408 | 1,425,408 |

Cgroup readings include pre-existing workspace memory/file cache; the raw report
also records each pre-run memory breakdown. They are not process-only allocation
counts. All resource readings are sampled separately from the timing gate. CPU
and memory limits and memory-event counters remain unchanged; no new OOM/kill
events occurred.


## Evidence and CI

`results.json` records metrics and resource readings. The archive retains all
raw samples, failed and successful checks, both control experiments, final
control, unit source fingerprints, assembly and source snapshots. Executable
binaries, build caches and generated workload files are excluded. Binary hashes
and runner scripts identify the measured artifacts; `evidence.sha256` verifies
the archive.

Local performance qualification is separate from remote CI. At the preceding
`dad27af` head, Rust CodeQL still failed its findings gate and Kani default was
cancelled during resource mutations; Kani all and the ordinary platform tests
passed. This optimization does not waive those checks. No merge or release.
