# Audit follow-up qualification

This follow-up starts at PR #18 head
`38c37d0e4d59493aa85abdfc1e4955e9fc3cec7c`. The 23 preceding PR commits were
reviewed before editing. In particular, `819433c` recovered chunk/rewrite
performance through buffer reuse and output page advice; `ed92ba1` moved frame
size checks before reservations; `e5e81a1` preserved amortized boundary discovery
and borrowed buffer budgets; `38c37d0` repaired resumed-writer durability.
Those behaviors and the previously disclosed pre-hardening costs remain relevant.

## Changes and allocation evidence

The zstd reservation screen now derives an upper bound from complete blocks.
Raw and run-length encoded blocks contribute their block lengths, bounded by
128 KiB; compressed blocks contribute at most 128 KiB each. Frame content-size
hints and chunk-container lengths must fit that bound. Invalid/truncated framing
cannot fall back to reserving the caller's entire limit. Chunk output reservation
retains the established bulk decoding, scratch reuse and page advice after every
chunk passes this screen. Compressed contents and checksums still require actual
decoding: the screen is a conservative allocation bound, not content validation.

Legacy zstd layouts use the backend's existing safe `decompress_bound` binding,
whose legacy implementation counts complete blocks independently of content-size
hints. The zstd `experimental` dependency feature exposes that binding; no package
version, new dependency, public Rust signature or persisted format changes.
The unchanged upstream zstd 1.5.7 legacy/current concatenation fixture is retained
with its BSD license. Encoder-produced raw, repetitive and mixed-byte frames,
known/unknown sizes, checksums, concatenation, skippable frames and block-boundary
sizes also have regression coverage.

Allocation refusal uses a synthetic Rust allocator. With a 4 MiB caller limit,
the baseline requested a 4 MiB allocation for both the 13-byte inflated frame and
the 7-byte invalid input. The candidate requests neither. A matching forged inner
frame and outer chunk-container length is also rejected before the full output
reservation. The same chunk regression test deliberately fails on the baseline with one refused large request; that negative-control log is retained. **No actual OOM process termination was reproduced.**

The advisory/documentation unit changes no runtime source. It audits package
identities read from `vendor/*/Cargo.toml` separately from lockfiles and uses a
synthetic RustSec advisory to prove detection of the affected redb identity and
acceptance of the patched identity. `SECURITY.md` describes external modification
and truncate hazards for mapped files without changing the unsafe APIs.

Disposable row, packed metadata, merge-byte and scratch-index files now share
spill placement and optional per-file capacity policy. Defaults retain platform
temporary storage with no additional cap. Explicit directory failures propagate;
limited index failures discard only newly appended unpublished data and preserve
prior roots. Unlimited spills retain `File::write_all`; limited complete writes check the extent once and track partial progress. An isolated Unix RLIMIT_FSIZE test verifies retry after an actual partial kernel write. Atomic replacement files remain beside their destination. See
[SECURITY.md](../../../SECURITY.md) for configuration and the distinction between
a per-file cap, aggregate quotas and tmpfs memory.

## Validation

Final local validation and commands are recorded in `validation.json`. Results
are scoped to the tested Linux environment; new remote CI results are separate.

- 1,322 workspace/documentation tests passed; 30 existing ignored tests.
- All 30 required release storage/resource contracts passed, including the two
  allocation-refusal and two spill-policy contracts added here.
- 40 Python tests, warnings-denied Clippy, no-default-feature compilation,
  locked public-API/rename fixtures, formatting and actionlint passed.
- Six lockfile audits and the explicit vendored redb 4.4.0 identity audit found
  zero vulnerabilities against the recorded RustSec database. Dependency bans,
  licenses and sources passed; redb remains outside the runtime graph.
- ASan-enabled `codec_arbitrary` and `native_arbitrary` fuzz smoke runs completed
  4,202,060 and 319,022 executions respectively, with the existing 4,096 MiB RSS
  limit. The native smoke was repeated after the final spill fast path; the codec implementation is unchanged since its smoke run. The earlier 331,167 native executions are also retained. The codec target now includes bounded chunk decoding and valid/inflated
  zstd seeds. These are smoke runs, not exhaustive proofs.

## Performance method

All release binaries use Rust 1.95.0 and all features. Runtime revisions have
separate build directories. Builds/tests/fuzzing finish before measurements.
[provenance.json](provenance.json) records local Git trees and harness hashes; [published-runtime-revisions.json](published-runtime-revisions.json) maps them to identical GitHub source trees. Concurrent reports record the runner checkout in `git_head`; their executable hashes identify the actual compared runtime.
The cgroup remains at four CPU equivalents and 16 GiB; application cache, batch,
read limits and boundary-buffer loans are unchanged. Native runs use one fixed
CPU; concurrent runs use the available CPU set. `host.json` records the executor.

The native suite includes two million records, random/reverse liveness changes,
recovery, complex rewrite/checkpoint operations, approximately 4 GiB files and
64 MiB chunk codecs. Standalone decoder cases cover known/unknown sizes and
concatenated frames. Before/after work counters, output bytes/CRC and temporary
lengths must match. The documentation/advisory unit also has same-binary A/A
measurements, with identical executable hashes.

Concurrent 1 and 4 GiB cases use eight following readers plus one pinned reader,
with 16,384 checks per following reader per mutation epoch. Reader work, snapshots
and trace hashes must match. `compare_load.py` retains the existing 5% wall/CPU/
maximum-RSS/write gate. Native/standalone reports retain named-operation time,
whole-process CPU, maximum RSS and writes; measured increases are reported.

Scratch for measurements is on `/workspace`'s overlay filesystem, not `/tmp`'s
tmpfs. RSS excludes some system memory used by filesystem caching and tmpfs.
`resource_probe.py` separately samples open regular files and deleted temporary-file
lengths/allocated blocks every 5 ms, plus process/cgroup memory. These sampled
peaks can miss shorter-lived maxima and are not substituted for hard capacity
checks. Probe timings are excluded from the performance comparisons.

## Results

Positive values are increases. Time and CPU use medians; RSS uses the maximum of all observations. Zero write deltas include cases with no writes. Every initial and confirmation observation remains in `measurements/`.

### Compression unit: original head → block-bound screen

| Workload | Pairs | Operation before → after (ms) | Operation | Process CPU | Maximum RSS | Writes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 4 GiB large / 4,096 reverse marks | 20 | 12.418 → 12.247 | -1.38% | +1.05% | +0.12% | -0.00% |
| Complex rewrite / 65,536 records | 5 | 604.738 → 587.108 | -2.92% | +0.88% | +0.70% | +0.00% |
| 64 MiB chunk encode ×20 | 5 | 227.206 → 223.667 | -1.56% | -1.82% | -0.09% | 0.00% |
| 64 MiB chunk decode ×20 | 5 | 332.124 → 341.037 | +2.68% | +1.72% | +0.05% | 0.00% |

### Spill unit: compression revision → final revision (default settings)

| Workload | Pairs | Operation before → after (ms) | Operation | Process CPU | Maximum RSS | Writes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2M fixed / 200k forward marks | 20 | 168.580 → 170.937 | +1.40% | -0.38% | +0.00% | +0.00% |
| 200k variable / 20k random marks | 20 | 38.873 → 38.715 | -0.41% | -1.85% | -0.37% | +0.00% |
| 200k variable random marks after rewrite | 5 | 69.162 → 72.569 | +4.93% | +0.43% | +0.00% | +0.00% |
| 200k-record recovery | 5 | 423.027 → 423.619 | +0.14% | +0.75% | -0.55% | +0.00% |
| 4 GiB large / 4,096 reverse marks | 20 | 12.749 → 12.919 | +1.33% | -1.09% | -0.23% | -0.00% |
| 4 GiB large / lazy reverse marks | 20 | 87.011 → 88.968 | +2.25% | -0.53% | +0.00% | +0.00% |
| 4 GiB mixed / 8,192 random marks | 5 | 41.735 → 39.941 | -4.30% | +2.74% | -0.17% | -0.01% |
| Complex rewrite / 65,536 records | 5 | 590.961 → 596.495 | +0.94% | -0.62% | +0.19% | +0.00% |
| Checkpoint rewrite / 200k records | 20 | 1680.031 → 1708.373 | +1.69% | -0.71% | +0.02% | +0.00% |
| 64 MiB chunk encode ×20 | 20 | 206.717 → 208.810 | +1.01% | +0.18% | +0.14% | 0.00% |
| 64 MiB chunk decode ×20 | 5 | 379.002 → 375.923 | -0.81% | -2.00% | +0.09% | 0.00% |

### Explicit spill directory and 32 MiB per-file limit

| Workload | Pairs | Operation before → after (ms) | Operation | Process CPU | Maximum RSS | Writes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 2M fixed / 200k forward marks | 3 | 167.213 → 172.402 | +3.10% | +3.14% | -2.36% | -0.00% |
| 200k variable random marks after rewrite | 18 | 72.154 → 73.751 | +2.21% | +3.71% | -0.12% | +0.00% |
| Complex rewrite / 65,536 records | 3 | 587.283 → 585.514 | -0.30% | +1.75% | -0.42% | -0.01% |

### Signals and confirmation

- codec-native, 4 GiB large / 4,096 reverse marks, operation_seconds: initial +8.88%, additional 15-pair confirmation -4.41%, all 20 pairs pooled -1.38%.
- spill-native, 200k variable / 20k random marks, operation_seconds: initial +8.12%, additional 15-pair confirmation -2.86%, all 20 pairs pooled -0.41%.
- spill-native, 4 GiB large / 4,096 reverse marks, operation_seconds: initial +15.12%, additional 15-pair confirmation -3.23%, all 20 pairs pooled +1.33%.
- spill-native, 4 GiB large / lazy reverse marks, operation_seconds: initial +5.10%, additional 15-pair confirmation +1.72%, all 20 pairs pooled +2.25%.
- spill-native, Checkpoint rewrite / 200k records, operation_seconds: initial +6.87%, additional 15-pair confirmation +1.21%, all 20 pairs pooled +1.69%.
- spill-native, 64 MiB chunk encode ×20, operation_seconds: initial +13.92%, additional 15-pair confirmation +0.02%, all 20 pairs pooled +1.01%.
- spill-native, 64 MiB chunk encode ×20, process_cpu_seconds: initial +10.77%, additional 15-pair confirmation +0.17%, all 20 pairs pooled +0.18%.
- spill-policy, 200k variable random marks after rewrite, operation_seconds: initial +8.46%, additional 15-pair confirmation +0.54%, all 18 pairs pooled +2.21%.

The revised unlimited-spill path also received 15 additional pairs in the previously regressed two-million-record case: confirmation CPU -0.81%, all 20 pairs pooled CPU -0.38%. The superseded wrapper cost of +6.37% remains recorded below.

Same-binary A/A data are retained in `audit-docs-aa` and `noise-aa-confirmation`. The initial unchanged-binary 4 GiB case moved −15.02% in its short operation and +5.13% in process CPU; this demonstrates measurement variability, not an implementation improvement. It does not erase the A/B observations.

### Standalone zstd decoding

The retained identical-harness repetitions use five initial pairs per row, plus 15 confirmation pairs for the flagged 1 MiB spill comparison. The earlier formatting-only compression harness run is also retained; initial spill-wrapper trials are under `superseded/spill-v1/`. Encoded CRCs and decoded bytes match in every comparison.

| Unit | Input | Pairs | Operation | CPU | Maximum RSS |
| --- | --- | ---: | ---: | ---: | ---: |
| Compression | known 1 MiB | 5 | +3.51% | +3.47% | +0.10% |
| Compression | known 64 MiB | 5 | +0.61% | -0.05% | -0.02% |
| Compression | unknown 64 MiB | 5 | +1.25% | +1.64% | -0.01% |
| Compression | concatenated 64 MiB | 5 | +1.15% | +0.69% | -0.02% |
| Spill | known 1 MiB | 20 | +0.35% | +0.14% | +0.00% |
| Spill | known 64 MiB | 5 | +1.64% | +1.30% | +0.07% |
| Spill | unknown 64 MiB | 5 | -1.89% | -1.10% | -0.01% |
| Spill | concatenated 64 MiB | 5 | -0.42% | -0.12% | +0.01% |

Standalone spill case 0: initial operation +7.53%, CPU +7.19%; confirmation operation -0.27%, CPU -0.09%. The table pools all observations.

### Single writer with concurrent readers

Three alternating pairs per size and unit; fixed reader work, snapshots and traces match.

| Unit | Size | Wall | CPU | Maximum RSS | Writes | Gate |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| Compression | 1 GiB | -0.22% | -1.24% | +0.17% | -0.00% | passed |
| Compression | 4 GiB | -0.68% | +0.34% | +0.12% | -0.00% | passed |
| Spill | 1 GiB | +1.55% | -0.37% | +0.22% | -0.00% | passed |
| Spill | 4 GiB | +4.21% | +1.77% | -0.02% | -0.00% | passed |

### Temporary storage and memory

Separate 5 ms probes below report maximum observed lengths/allocated bytes for deleted files whose basename starts with `#` or `.tmp`. This filename heuristic includes unnamed private scratch and may include replacement temporaries; it excludes retired database generations with ordinary basenames. All open-file allocations and all deleted-file lengths are also recorded, so the selected temporary names are not total filesystem demand. A sampling result of zero does not prove that no shorter-lived file existed.

| Workload | Original logical / allocated | Compression logical / allocated | Final logical / allocated |
| --- | ---: | ---: | ---: |
| forward | 31,457,280 / 31,457,280 | 31,457,280 / 31,457,280 | 31,457,280 / 31,457,280 |
| rewrite | 4,144,723 / 4,149,248 | 4,144,723 / 4,149,248 | 4,144,723 / 4,149,248 |
| large | 0 / 0 | 0 / 0 | 0 / 0 |
| complex | 4,194,190 / 4,194,304 | 4,194,190 / 4,194,304 | 4,194,190 / 4,194,304 |

| Workload | Original open-file allocation peak (MiB) | Compression peak (MiB) | Final peak (MiB) |
| --- | ---: | ---: | ---: |
| forward | 167.340 | 167.340 | 167.340 |
| rewrite | 89.035 | 89.035 | 89.035 |
| large | 4101.512 | 4101.512 | 4101.512 |
| complex | 87.695 | 87.695 | 87.695 |

All probe commands exited successfully. `resources/` also records RSS, all open regular-file blocks, deleted generation lengths and cgroup memory, including the explicit-cap complex run. Native logical temporary lengths and before/after work counters matched; concurrent write-byte differences are shown above.

The session cgroup reached its unchanged 16 GiB limit during this overall development/measurement session (including filesystem cache); its cumulative OOM and OOM-kill counters remained zero. These cumulative counters are not isolated benchmark peaks. Per-run cgroup peaks include shared cache and unrelated executor memory and are not application-only RSS. No resource limit was raised to accommodate this.

## Superseded allocation experiments

Two exploratory implementations grew output buffers progressively. The first
increased the 64 MiB chunk-decode operation median by 152.19% and process CPU by
117.23%. Larger growth steps reduced those increases to 13.95% and 11.19%, but
still missed the existing performance comparison threshold. Neither is the
candidate implementation. Their source patches, observations and binary hashes
are retained under `superseded/`; the interrupted first concurrent trial is
explicitly incomplete. The final block-bound screen retains the bulk path.

The initial spill wrapper also had a repeatable CPU cost in the two-million-record fixture: +7.26% initially, +5.25% in 15 additional pairs, and +6.37% across all 20 pairs. It was superseded by preserving the native complete-write path for unlimited spills and checking limited complete-write extents once. All initial spill measurements, including both concurrent sizes and the optional-cap runs, remain in `superseded/spill-v1/`. The final spill tables use the revised wrapper.

The first resource probe matched `.tmp` anywhere in a deleted pathname, so a temporary **directory** could misclassify a retired database generation as anonymous scratch. Those observations and the original probe remain in `superseded/spill-v1/resources/` and `superseded/resource-probe-v1.py`; their `anonymous_scratch_*` names are heuristic counts, not exact disposable-spill totals. The final probe matches the basename, labels the selected files explicitly, and also retains total open-file allocations and all deleted-generation lengths.

## Reproduction

Build the recorded runtime revisions with identical example harnesses:

```sh
cargo build --locked --release --all-features -p varlit \
  --example oom_maintenance_load --example record_mutation_load --example scalable_load
cargo build --locked --release --all-features -p varlit-core --example decompression_load
python3 docs/performance/audit-followup-20261102/bench.py \
  --before BEFORE_EXAMPLES --after AFTER_EXAMPLES --repo . --output NATIVE \
  --phase native --pairs 5
python3 docs/performance/audit-followup-20261102/bench.py \
  --before BEFORE_EXAMPLES --after AFTER_EXAMPLES --repo . --output SWMR \
  --phase concurrent --pairs 3
python3 docs/performance/audit-followup-20261102/extra.py \
  --before BEFORE_EXAMPLES --after AFTER_EXAMPLES --repo . --output CODEC --pairs 5
python3 scripts/check_vendored_advisories.py --self-test
python3 scripts/check_vendored_advisories.py
```

The optional policy comparison sets `VARLIT_SPILL_DIR` to an existing directory
on the same filesystem and `VARLIT_SPILL_MAX_BYTES=33554432`. It adds a constraint;
it does not increase a cache/buffer allowance or cap database file size.
