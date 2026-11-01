# Dependency update qualification — 2026-11-02 (KST)

Baseline: `7e4396196fc4672487e7e7c84d4750f8e133f977`. This update refreshes
compatible published dependencies and ports the test-only redb oracle to 4.4.0.
Varlit's production Rust sources, public API, persisted format, cache budgets
and batch limits are unchanged. The Varlit crate versions remain 0.12.1. Functional checks passed; both equal-work
SWMR sizes passed the existing resource gate. An initial CPU increase did not
recur in the fixed confirmation experiment; all original samples are retained.

## Updated resolution

| Package | Before | After |
| --- | --- | --- |
| cc | 1.5.1 | 1.10.0 |
| equivalent | 1.0.2 | 1.0.3 |
| find-msvc-tools | 0.1.14 | 0.1.15 |
| rustix | 1.1.5 | 1.1.6 |
| serde_json | 1.0.151 | 1.0.154 |
| serde_spanned | 1.1.1 | 1.1.2 |
| toml | 1.1.6 | 1.1.8 |
| toml_datetime | 1.1.1 | 1.1.2 |
| toml_edit | 0.25.15 | 0.25.17 |
| toml_parser | 1.1.3 | 1.1.5 |
| toml_writer | 1.1.2 | 1.1.3 |
| unicode-ident | 1.0.26 | 1.0.27 |
| arbitrary (fuzz) | 1.4.2 | 1.5.0 |
| redb (vendored test oracle) | 4.3.0 | 4.4.0 |

TOML package versions retain their `+spec-1.1.0` suffix. The 13 registry
updates overlap across the root, fuzz, public-API and rename workspaces; they
are not 13 distinct updates in each workspace. Root selects 12 plus redb;
fuzz selects nine plus redb; the fixtures select eight and six. Loom is unchanged.
The standalone redb test workspace additionally refreshes 24 registry package
entries. [Every lockfile delta and checksum](lockfile-changes.json) is retained,
including its existing redb 2.6.0 backward-compatibility oracle and the added
syn 3 dependency alongside syn 2.

The [parent-release review](../../dependency-review-20261101.md) covers the
five incompatible replacements. Their latest published parents still exclude
the newer child major versions. They retain supported resolution; this update
does not introduce unreleased Git dependencies or dependency overrides.

## redb source port

The crates.io 4.4.0 archive checksum was verified before merging the local
4.3.0 patch against pristine 4.3.0 and 4.4.0 sources. Upstream commit:
`7061f7e60d4573311c0636e161d417b93e9fd2b1`. Upstream changes include bounded
page-cache queue growth, compaction/write-transaction waiting, abort/integrity
handling and non-durable commit preservation after a caught panic.

The local two-root snapshot extension retains independent transaction pins,
a bounded savepoint summary and read-only registration on exclusive open.
Its helper signatures follow upstream's now-unconditional header/lock plumbing;
experimental public API gates are preserved. Existing local snapshot regressions
and new upstream tests run together. See [local patch notes](../../../vendor/redb/VARVE-PATCH.md).

redb remains a development/fuzz oracle. The runtime dependency guard confirms
that redb and verification tools are absent from the published runtime graph.
The standalone `redb-derive 0.1.0` restoration is retained for the packaged example.

## Functional and resource validation

- Workspace/unit/integration/doc tests: **1,308 passed, 29 existing ignored**;
  the project's test runner verified temporary-artifact cleanup.
- Standalone redb: **629 passed**, including six local snapshot regressions.
  `just test` also passed with warnings-denied Clippy, formatting, licenses and
  advisories. Its offline container used Rust 1.95.0 and the Docker runner
  documented in the local patch notes; native all-feature tests passed separately.
- **21 release-mode storage/resource contracts** passed, including bounded
  discovery, linear recovery, OOM propagation and source-integrity checks.
- **33 Python tests**, workspace warnings-denied Clippy, no-default-feature
  compilation and both locked public-API/name-renaming fixtures passed.
- **Two Kani harnesses**, **two Loom production-module models**, **six Miri codec
  tests** and **two fuzz-support regression tests** passed.
- Six lockfile `cargo audit` checks reported zero advisories against database
  commit `2295466c4e9df26647fd67d8fa1c705ac2fc7f81` (2026-10-30).
  Root/fuzz license and dependency-policy checks passed. This count is specific
  to that database; the upstream patch review also identified fixes beyond it.
- Six CycloneDX inventories were regenerated for default/all-feature resolution
  of the three publishable crates without changing locked resolution.

The previous PR Sanitizers run failed on
`indexed::tests::rebuild_refuses_truncated_record_instead_of_publishing_a_prefix`.
The exact test reproduces `unsupported operation: fsync is only supported on
file-backed file descriptors` under the pinned Miri nightly. Its directory-sync
operation is now documented in the exact-name Miri exclusion list; native and
ASan coverage remain. The selection guard validates 198 selected tests and 49 documented exclusions
out of 247 enumerated tests. The exact test passed natively; the complete remote
Miri job must rerun on the final commit.

All **13 AddressSanitizer fuzz targets passed**, using the generated corpus and
seed `20261102`: 30 seconds per target and 120 seconds for
`native_redb_differential` (libFuzzer's completed durations were 31/121 seconds).
The total was **3,061,386 executions**, including **18,041 native/redb comparisons**.
Per-target counts, log hashes, audit metadata and contract names are retained in
[validation.json](validation.json). The native C/zstd build and fuzz binaries use
the updated cc toolchain dependency.

## Performance method

Both variants use Rust 1.95.0 release builds in separate target directories, the
same feature flags and byte-identical benchmark harnesses. The baseline is the
pre-update PR head above, not the older pre-hardening implementation. Builds,
tests and fuzzing finished before timing began. Eleven native workloads and two
indexed single-writer/multiple-reader (SWMR) workloads each use five alternating
before/after pairs. Single-process workloads are pinned to one available CPU;
SWMR uses the same four-CPU quota for both builds. Scratch files are disk-backed.

The native cases cover two million records, variable/reverse/random mutation,
rewritten records, recovery, approximately 4 GiB files, complex maintenance,
checkpoints and 64 MiB chunk encode/decode. Native output geometry, scan counters
and temporary-file sizes must match exactly. Operation time is reported separately
from whole-process CPU so fixture creation cannot hide a slower operation.

The indexed cases use 1 GiB/16 MiB epochs and 4 GiB/64 MiB epochs, eight following
readers plus one pinned reader, and 16,384 checks per reader per mutation epoch.
The existing comparison verifies equal read counts, snapshots, trace hashes,
cache/batch arguments and environment. Its unchanged 5% gate covers median wall
time, median CPU time, maximum resident set size (RSS) and median write bytes.
The separate one-second random-read phase is time-bounded; its operation counts
are not used as evidence of equal work.

## Measured results

Initial native comparisons (five pairs; operation times are milliseconds):

| Workload | Before | After | Operation | Whole-process CPU | Max RSS |
| --- | ---: | ---: | ---: | ---: | ---: |
| 2M fixed records / 200k forward marks | 186.925 | 185.954 | -0.52% | +10.57% | -0.21% |
| 200k variable / 20k random marks | 41.955 | 41.107 | -2.02% | -1.92% | -0.64% |
| After rewrite: 200k variable / 20k random | 76.372 | 75.230 | -1.50% | +3.73% | +0.00% |
| 200k-record recovery | 483.772 | 471.959 | -2.44% | +4.01% | +0.00% |
| 4 GiB large / 4,096 reverse marks | 14.361 | 13.407 | -6.65% | -1.69% | +2.12% |
| 4 GiB large / lazy reverse marks | 95.223 | 90.096 | -5.38% | -2.38% | +0.00% |
| 4 GiB mixed / 8,192 random marks | 55.228 | 50.842 | -7.94% | -0.23% | +0.00% |
| Complex maintenance (65,536 x 512 B) | 608.912 | 601.496 | -1.22% | +1.36% | +0.65% |
| Checkpoints (200k x 256 B) | 1693.959 | 1720.436 | +1.56% | +4.59% | -0.33% |
| 64 MiB chunk encode, 20 iterations | 209.022 | 212.999 | +1.90% | +1.95% | -0.01% |
| 64 MiB same-input decode, 20 iterations | 462.589 | 383.606 | -17.07% | -11.89% | +0.01% |

The initial 2M-record whole-process CPU median increased 10.57%, while its
mark operation median changed -0.52%. This triggered a fixed follow-up
of 15 additional alternating A/B pairs and ten same-baseline-binary A/A pairs.
The original samples remain in every report; pooled A/B results include all
20 pairs. The confirmation also records user/system CPU separately.

| 2M-record comparison | Pairs | Before CPU (s) | After CPU (s) | CPU change | Operation change |
| --- | ---: | ---: | ---: | ---: | ---: |
| initial | 5 | 3.9831 | 4.4039 | +10.57% | -0.52% |
| confirmation | 15 | 4.2089 | 4.0774 | -3.12% | -7.47% |
| pooled | 20 | 4.2084 | 4.0993 | -2.59% | -6.14% |
| same binary control | 10 | 4.1877 | 4.1791 | -0.21% | +3.47% |

Indexed SWMR write-phase comparisons (five pairs per size):

| Payload | Wall time | CPU | Maximum RSS | Written bytes | Existing 5% gate |
| --- | ---: | ---: | ---: | ---: | --- |
| 1 GiB | -0.80% | -0.62% | -0.11% | +0.0047% | passed |
| 4 GiB | -0.63% | -0.20% | -0.06% | -0.0007% | passed |

All native pairs had identical logical file sizes, temporary-file lengths
and frame/work counters. Compare physical write counts separately in the raw
data: filesystem block accounting can vary even with identical logical lengths.
RSS measures process-resident memory and does not include the complete filesystem
page cache. Configured cache/batch limits and disk-backed scratch were unchanged.

The retained measurements cover 110 initial native observations, 20 SWMR
observations and 50 CPU confirmation/control observations. Neither the large
decode speedup in the first group nor small positive differences establish a
general performance improvement. This is a local Linux qualification up to about
4 GiB, not a terabyte-scale or cross-platform measurement. Existing security
hardening costs relative to pre-hardening code are in the separate boundary
redesign report; this comparison isolates the dependency follow-up.

## Reproduction and evidence

Build both revisions using the command in [provenance.json](provenance.json),
then run `bench.py --phase native` and `bench.py --phase concurrent` with the
two `release/examples` directories and one empty output directory. After those
finish, run `confirm-cpu.py` with the two `record_mutation_load` executable paths
and the same output directory. Do not run compilers/tests concurrently.

- [Native raw samples](native-samples.json) and [summary](native-summary.json).
- [CPU confirmation/control samples](cpu-confirmation-samples.json) and [summary](cpu-confirmation-summary.json).
- [1 GiB comparison](swmr-1-comparison.json), [4 GiB comparison](swmr-4-comparison.json), and all 20 complete reports under `swmr/` (JSON whitespace compacted only).
- [Binary hashes](binary-hashes.json), [lockfile changes](lockfile-changes.json), [validation](validation.json), and [Miri failure reproduction](miri-ci-reproduction.json).

Remote CI, Windows/macOS execution and CodeQL must be evaluated on the published
commit separately from these local results. No merge or crate publication is included.
